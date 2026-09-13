//! Cache lookup, input revalidation, and transactional output restoration.

use std::{fs::File, time::Instant};

use octa_cache::{BlobReader, CacheError, RestoreOutcome};
use octa_cache_protocol::{CachedArtifact, CachedReport, Digest};
use octa_output::CacheReason;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

use super::{
  identity::action_digest,
  replay::{cached_result, validate_materialized_resources, validate_resource_contract},
  PublicationIdentity, ResultCache, TaskCachePlan, TaskCacheState,
};
use crate::{
  error::{ExecutorError, ExecutorResult},
  execution_result::CacheOutcome,
  runtime_output::RuntimeOutput,
  task::RuntimeContext,
};

/// Per-node values consumed during one cache lookup.
pub(crate) struct CacheLookup<'a> {
  pub(crate) plan: &'a TaskCachePlan,
  pub(crate) state: &'a TaskCacheState,
  pub(crate) plugin_manager: &'a octa_plugin_manager::plugin_manager::PluginManager,
  pub(crate) context: &'a RuntimeContext,
  pub(crate) output: &'a RuntimeOutput,
  pub(crate) cancel: &'a CancellationToken,
  pub(crate) dry: bool,
  pub(crate) force: bool,
}

/// Performs a best-effort lookup and verified restore.
pub(crate) async fn lookup(cache: &ResultCache, request: CacheLookup<'_>) -> ExecutorResult<()> {
  let CacheLookup {
    plan,
    state,
    plugin_manager,
    context,
    output,
    cancel,
    dry,
    force,
  } = request;
  if dry {
    state
      .set_lookup(None, None, CacheOutcome::bypassed(CacheReason::DryRun))
      .await;
    return Ok(());
  }
  if !context.vars.secret_names().is_empty() {
    state
      .set_lookup(None, None, CacheOutcome::bypassed(CacheReason::SecretVariables))
      .await;
    return Ok(());
  }

  let started = Instant::now();
  let snapshot = match cache
    .snapshotter
    .snapshot_pattern_sets(&plan.workspace, &plan.input_pattern_sets, cancel)
    .await
  {
    Ok(snapshot) => snapshot,
    Err(octa_cache::CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache lookup".to_owned())),
    Err(error) => {
      return record_lookup_error(state, output, None, CacheReason::InputSnapshotFailed, error, started).await;
    },
  };
  let action = action_digest(cache, plan, plugin_manager, context, snapshot.root).await?;
  output.cache_lookup_started(action.to_string()).await?;

  if force {
    output.cache_miss(action.to_string(), CacheReason::Force).await?;
    state
      .set_lookup(
        Some(PublicationIdentity {
          action,
          input_root: snapshot.root,
        }),
        None,
        CacheOutcome::miss(action.to_string(), CacheReason::Force, started.elapsed()),
      )
      .await;
    return Ok(());
  }

  if !cache.access.can_read() {
    output.cache_miss(action.to_string(), CacheReason::ReadDisabled).await?;
    state
      .set_lookup(
        Some(PublicationIdentity {
          action,
          input_root: snapshot.root,
        }),
        None,
        CacheOutcome::miss(action.to_string(), CacheReason::ReadDisabled, started.elapsed()),
      )
      .await;
    return Ok(());
  }

  let lookup = match cache.store.get_action(&cache.namespace, &action).await {
    Ok(result) => result,
    Err(CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache lookup".to_owned())),
    Err(error) => {
      return record_lookup_error(state, output, Some(action), CacheReason::LookupFailed, error, started).await;
    },
  };
  let Some(lookup) = lookup else {
    output
      .cache_miss(action.to_string(), CacheReason::ActionNotFound)
      .await?;
    state
      .set_lookup(
        Some(PublicationIdentity {
          action,
          input_root: snapshot.root,
        }),
        None,
        CacheOutcome::miss(action.to_string(), CacheReason::ActionNotFound, started.elapsed()),
      )
      .await;
    return Ok(());
  };
  let layer = lookup.layer;
  let result = lookup.result;

  if let Some(bundle) = &result.output_bundle {
    if let Err(error) = cache.bundle_limits.validate_descriptor(bundle) {
      return record_lookup_error(state, output, Some(action), CacheReason::RestoreFailed, error, started).await;
    }
  }
  if let Err(error) = validate_resource_contract(plan, &result.artifacts, &result.reports) {
    return record_lookup_error(
      state,
      output,
      Some(action),
      CacheReason::ResourceContractInvalid,
      error,
      started,
    )
    .await;
  }

  if state.is_probe().await {
    if let Some(bundle) = &result.output_bundle {
      match cache.store.find_missing_blobs(std::slice::from_ref(bundle)).await {
        Ok(missing) if missing.is_empty() => {},
        Ok(_) => {
          return record_lookup_error(
            state,
            output,
            Some(action),
            CacheReason::BlobReadFailed,
            "cache action references a missing local blob",
            started,
          )
          .await;
        },
        Err(CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache lookup".to_owned())),
        Err(error) => {
          return record_lookup_error(
            state,
            output,
            Some(action),
            CacheReason::BlobLookupFailed,
            error,
            started,
          )
          .await;
        },
      }
    }
    let captured = cached_result(result);
    let outcome = CacheOutcome::hit(action.to_string(), layer, 0, started.elapsed());
    output.cache_hit(action.to_string(), 0).await?;
    state.set_lookup(None, Some(captured), outcome).await;
    return Ok(());
  }

  // Recheck immediately before replacing outputs. A concurrent source change
  // turns the lookup into a miss rather than restoring a result for stale input.
  let verified = match cache
    .snapshotter
    .snapshot_pattern_sets(&plan.workspace, &plan.input_pattern_sets, cancel)
    .await
  {
    Ok(snapshot) => snapshot,
    Err(octa_cache::CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache lookup".to_owned())),
    Err(error) => {
      return record_lookup_error(
        state,
        output,
        Some(action),
        CacheReason::InputRecheckFailed,
        error,
        started,
      )
      .await;
    },
  };
  if verified.root != snapshot.root {
    output
      .cache_miss(action.to_string(), CacheReason::InputsChangedDuringLookup)
      .await?;
    // The command may observe either snapshot while the workspace is changing.
    // Execute normally, but do not publish under either identity.
    state
      .set_lookup(
        None,
        None,
        CacheOutcome::miss(
          action.to_string(),
          CacheReason::InputsChangedDuringLookup,
          started.elapsed(),
        ),
      )
      .await;
    return Ok(());
  }

  let mut restored_bytes = 0;
  if let Some(bundle) = &result.output_bundle {
    let reader = match cache.store.read_blob(bundle).await {
      Ok(reader) => reader,
      Err(CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache restore".to_owned())),
      Err(error) => {
        return record_lookup_error(state, output, Some(action), CacheReason::BlobReadFailed, error, started).await;
      },
    };
    let restored =
      match restore_cached_bundle(cache, plan, bundle, reader, cancel, &result.artifacts, &result.reports).await {
        Ok(restored) => restored,
        Err(CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache restore".to_owned())),
        Err(error) => {
          return record_lookup_error(state, output, Some(action), CacheReason::RestoreFailed, error, started).await;
        },
      };
    restored_bytes = restored;
  } else if !plan.outputs.is_empty() {
    return record_lookup_error(
      state,
      output,
      Some(action),
      CacheReason::MissingOutputBundle,
      "cache result omitted declared outputs",
      started,
    )
    .await;
  }

  // Only the store knows whether a verified hit needs internal promotion. The
  // executor deliberately reports the lifecycle point without branching on a
  // concrete tier or repeating remote publication.
  match cache.store.commit_verified_hit(&cache.namespace, &result).await {
    Ok(()) => {},
    Err(CacheError::Cancelled) => return Err(ExecutorError::TaskCancelled("cache restore".to_owned())),
    Err(error) => {
      tracing::warn!(cache.operation = "commit verified hit", error = %error, "cache hit promotion failed")
    },
  }

  let captured = cached_result(result);
  let outcome = CacheOutcome::hit(action.to_string(), layer, restored_bytes, started.elapsed());
  output.cache_hit(action.to_string(), restored_bytes).await?;
  output
    .cache_restore_finished(action.to_string(), restored_bytes)
    .await?;
  state.set_lookup(None, Some(captured), outcome).await;
  Ok(())
}

/// Retries only content-originated validation failures. Workspace I/O and path
/// failures describe the restore destination, so replacing the source blob
/// cannot fix them and must not quarantine a valid cache object.
async fn restore_cached_bundle(
  cache: &ResultCache,
  plan: &TaskCachePlan,
  descriptor: &octa_cache_protocol::BlobDescriptor,
  reader: BlobReader,
  cancel: &CancellationToken,
  artifacts: &[CachedArtifact],
  reports: &[CachedReport],
) -> Result<u64, CacheError> {
  match restore_bundle(cache, plan, descriptor, reader, cancel, artifacts, reports).await {
    Ok(bytes) => Ok(bytes),
    Err(error) if matches!(error, CacheError::InvalidBundle(_) | CacheError::BundleSource(_)) => {
      let Some(reader) = cache.store.recover_corrupt_blob(descriptor).await? else {
        return Err(error);
      };
      restore_bundle(cache, plan, descriptor, reader, cancel, artifacts, reports).await
    },
    Err(error) => Err(error),
  }
}

async fn record_lookup_error(
  state: &TaskCacheState,
  output: &RuntimeOutput,
  action: Option<Digest>,
  reason: CacheReason,
  error: impl std::fmt::Display,
  started: Instant,
) -> ExecutorResult<()> {
  let action = action.map(|action| action.to_string());
  output.cache_error(action.clone(), reason, error.to_string()).await?;
  state
    .set_lookup(None, None, CacheOutcome::error(action, reason, started.elapsed()))
    .await;
  Ok(())
}
async fn restore_bundle(
  cache: &ResultCache,
  plan: &TaskCachePlan,
  descriptor: &octa_cache_protocol::BlobDescriptor,
  reader: BlobReader,
  cancel: &CancellationToken,
  artifacts: &[CachedArtifact],
  reports: &[CachedReport],
) -> Result<u64, CacheError> {
  let temporary = NamedTempFile::new().map_err(CacheError::TemporaryFile)?;
  let mut sink = tokio::fs::File::from_std(temporary.reopen().map_err(CacheError::TemporaryFile)?);
  let copied = stage_encoded_blob(
    reader,
    &mut sink,
    descriptor.encoded_size_bytes,
    cache.bundle_limits.read_buffer_bytes,
    cancel,
  )
  .await?;
  if copied != descriptor.encoded_size_bytes {
    return Err(CacheError::InvalidBundle(format!(
      "cache blob transfer length is {copied}, expected {}",
      descriptor.encoded_size_bytes
    )));
  }
  sink.sync_all().await.map_err(CacheError::TemporaryFile)?;
  drop(sink);

  let restore = cache.restore.clone();
  let workspace = plan.workspace.clone();
  let roots = plan.outputs.clone();
  let descriptor = descriptor.clone();
  let expanded_size_bytes = descriptor.expanded_size_bytes;
  let cancel = cancel.clone();
  let artifacts = artifacts.to_vec();
  let reports = reports.to_vec();
  let path = temporary.path().to_owned();
  let restored = tokio::task::spawn_blocking(move || {
    let file = File::open(path).map_err(CacheError::TemporaryFile)?;
    restore.restore_validated(file, &descriptor, &workspace, &roots, &cancel, |materialization| {
      validate_materialized_resources(materialization, &artifacts, &reports)
    })
  })
  .await
  .map_err(CacheError::Worker)??;
  Ok(if restored == RestoreOutcome::Restored {
    expanded_size_bytes
  } else {
    0
  })
}

/// Copies one encoded representation into private staging while retaining I/O
/// provenance. Source failures may be retried from another cache tier;
/// temporary-file failures must be reported without quarantining good data.
async fn stage_encoded_blob(
  mut reader: BlobReader,
  sink: &mut tokio::fs::File,
  expected: u64,
  buffer_bytes: usize,
  cancel: &CancellationToken,
) -> Result<u64, CacheError> {
  let maximum = expected.saturating_add(1);
  let mut copied = 0_u64;
  let mut buffer = vec![0_u8; buffer_bytes];
  while copied < maximum {
    if cancel.is_cancelled() {
      return Err(CacheError::Cancelled);
    }
    let remaining = maximum - copied;
    let length = remaining.min(buffer.len() as u64) as usize;
    let read = reader
      .read(&mut buffer[..length])
      .await
      .map_err(CacheError::BundleSource)?;
    if read == 0 {
      break;
    }
    sink
      .write_all(&buffer[..read])
      .await
      .map_err(CacheError::TemporaryFile)?;
    copied += read as u64;
  }
  Ok(copied)
}
