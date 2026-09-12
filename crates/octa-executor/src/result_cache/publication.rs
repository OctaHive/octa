//! Successful-miss capture and immutable cache publication.

use octa_cache::{pack_bundle, CacheError, WriteOutcome};
use octa_cache_protocol::Digest;
use octa_output::CacheReason;
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;

use super::{
  replay::{action_result, validate_registered_resources},
  CapturedResult, PublicationIdentity, ResultCache, TaskCachePlan, TaskCacheState,
};
use crate::{
  error::{ExecutorError, ExecutorResult},
  execution_result::{CacheOutcome, CacheStatus},
  runtime_output::RuntimeOutput,
  structured_output::CompletionOutputs,
};

/// Publishes a completed miss or returns the restored logical result of a hit.
pub(crate) async fn finalize(
  cache: &ResultCache,
  plan: &TaskCachePlan,
  state: &TaskCacheState,
  output: &RuntimeOutput,
  cancel: &CancellationToken,
  dry: bool,
) -> ExecutorResult<(String, CompletionOutputs, CacheOutcome)> {
  if let Some((captured, outcome)) = state.hit().await {
    return Ok(completed_result(&captured, outcome));
  }
  let Some((publication, captured, mut outcome)) = state.miss().await else {
    return Err(ExecutorError::CacheState("finalization ran before lookup"));
  };
  if state.is_probe().await {
    return Ok(completed_result(&captured, outcome));
  }
  if dry || outcome.status != CacheStatus::Miss {
    return Ok(completed_result(&captured, outcome));
  }
  if !cache.access.can_write() {
    return Ok(completed_result(&captured, outcome));
  }
  let Some(PublicationIdentity { action, input_root }) = publication else {
    return Ok(completed_result(&captured, outcome));
  };

  output.cache_publish_started(action.to_string()).await?;
  let current = match cache.snapshotter.snapshot(&plan.workspace, &plan.inputs, cancel).await {
    Ok(snapshot) => snapshot,
    Err(CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache publication".to_owned())),
    Err(error) => {
      return publication_error(
        &captured,
        outcome,
        output,
        action,
        CacheReason::InputRecheckFailed,
        error,
      )
      .await;
    },
  };
  if current.root != input_root {
    outcome.reason = Some(CacheReason::InputsChangedBeforePublish);
    output
      .cache_error(
        Some(action.to_string()),
        CacheReason::InputsChangedBeforePublish,
        "inputs changed before cache publication",
      )
      .await?;
    return Ok(completed_result(&captured, outcome));
  }

  if let Err(error) = validate_registered_resources(plan, &captured) {
    return publication_error(
      &captured,
      outcome,
      output,
      action,
      CacheReason::ResourceContractInvalid,
      error,
    )
    .await;
  }

  let bundle = if plan.outputs.is_empty() {
    None
  } else {
    let packed = match pack_outputs(cache, plan, cancel).await {
      Ok(packed) => packed,
      Err(CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache publication".to_owned())),
      Err(error) => {
        return publication_error(
          &captured,
          outcome,
          output,
          action,
          CacheReason::OutputCaptureFailed,
          error,
        )
        .await;
      },
    };
    let descriptor = packed.1;
    let missing = match cache.store.find_missing_blobs(std::slice::from_ref(&descriptor)).await {
      Ok(missing) => missing,
      Err(error) => {
        return publication_error(&captured, outcome, output, action, CacheReason::BlobLookupFailed, error).await;
      },
    };
    let blob_missing = match missing.as_slice() {
      [] => false,
      [missing] if missing == &descriptor => true,
      _ => {
        return publication_error(
          &captured,
          outcome,
          output,
          action,
          CacheReason::BlobLookupFailed,
          "cache store returned an invalid missing-blob response",
        )
        .await;
      },
    };
    if blob_missing {
      match cache.store.write_blob_if_absent(&descriptor, Box::pin(packed.0)).await {
        Ok(WriteOutcome::Written | WriteOutcome::AlreadyPresent) => {},
        Ok(WriteOutcome::Conflict) => {
          return publication_error(
            &captured,
            outcome,
            output,
            action,
            CacheReason::BlobConflict,
            "immutable blob publication conflict",
          )
          .await;
        },
        Err(error) => {
          return publication_error(
            &captured,
            outcome,
            output,
            action,
            CacheReason::BlobPublicationFailed,
            error,
          )
          .await;
        },
      }
    }
    Some(descriptor)
  };

  let result = match action_result(action, bundle, &captured) {
    Ok(result) => result,
    Err(error) => {
      // Cache metadata is an optimization boundary. A task that completed
      // successfully must remain successful when its replay metadata cannot be
      // represented within the protocol's limits.
      return publication_error(
        &captured,
        outcome,
        output,
        action,
        CacheReason::ResultMetadataInvalid,
        error,
      )
      .await;
    },
  };
  match cache.store.write_action_if_absent(&cache.namespace, &result).await {
    Ok(WriteOutcome::Written | WriteOutcome::AlreadyPresent) => {
      output.cache_published(action.to_string()).await?;
    },
    Ok(WriteOutcome::Conflict) => {
      return publication_error(
        &captured,
        outcome,
        output,
        action,
        CacheReason::NondeterministicResult,
        "nondeterministic cache result",
      )
      .await;
    },
    Err(error) => {
      return publication_error(
        &captured,
        outcome,
        output,
        action,
        CacheReason::PublicationFailed,
        error,
      )
      .await;
    },
  }
  Ok(completed_result(&captured, outcome))
}

fn completed_result(captured: &CapturedResult, outcome: CacheOutcome) -> (String, CompletionOutputs, CacheOutcome) {
  (
    captured.stdout.clone(),
    captured.outputs.clone().with_cache(outcome.clone()),
    outcome,
  )
}

async fn publication_error(
  captured: &CapturedResult,
  mut outcome: CacheOutcome,
  output: &RuntimeOutput,
  action: Digest,
  reason: CacheReason,
  error: impl std::fmt::Display,
) -> ExecutorResult<(String, CompletionOutputs, CacheOutcome)> {
  outcome.status = CacheStatus::Error;
  outcome.reason = Some(reason);
  output
    .cache_error(Some(action.to_string()), reason, error.to_string())
    .await?;
  Ok(completed_result(captured, outcome))
}
async fn pack_outputs(
  cache: &ResultCache,
  plan: &TaskCachePlan,
  cancel: &CancellationToken,
) -> Result<(tokio::fs::File, octa_cache_protocol::BlobDescriptor), CacheError> {
  let restore = cache.restore.clone();
  let workspace = plan.workspace.clone();
  let roots = plan.outputs.clone();
  let encoding = cache.bundle_encoding;
  let limits = cache.bundle_limits;
  let cancel = cancel.clone();
  tokio::task::spawn_blocking(move || {
    let _guard = restore.lock_outputs(&workspace, &roots)?;
    let temporary = NamedTempFile::new().map_err(CacheError::TemporaryFile)?;
    let writer = temporary.reopen().map_err(CacheError::TemporaryFile)?;
    let packed = pack_bundle(writer, &workspace, &roots, encoding, limits, &cancel)?;
    // Retain an opened handle so publication cannot race with removal or
    // replacement of the temporary path between packing and upload.
    let reader = temporary.reopen().map_err(CacheError::TemporaryFile)?;
    Ok((tokio::fs::File::from_std(reader), packed.descriptor))
  })
  .await
  .map_err(CacheError::Worker)?
}
