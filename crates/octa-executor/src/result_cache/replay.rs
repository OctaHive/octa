//! Capture and validation of logical task results stored beside output bundles.

use std::{collections::BTreeMap, path::Path};

use octa_cache::CacheError;
use octa_cache_protocol::{
  ActionResultV1, CachedArtifact, CachedReport, Digest, RelativePath, ACTION_RESULT_VERSION_V1,
};
use octa_output::{RegisteredArtifact, RegisteredReport};
use serde_json::Map;

use super::{CapturedResult, TaskCachePlan};
use crate::{
  error::{ExecutorError, ExecutorResult},
  structured_output::{CompletionOutputs, TaskOutputs},
};

pub(super) fn aggregate(
  captures: &BTreeMap<usize, CapturedResult>,
  resources: Option<&CompletionOutputs>,
) -> CapturedResult {
  // Preserve the same declaration-order stdout separator used by ordinary
  // nested execution so a miss and a later hit have identical logical output.
  let stdout = captures
    .values()
    .map(|capture| capture.stdout.as_str())
    .collect::<Vec<_>>()
    .join("\n");
  let mut task = TaskOutputs::default();
  let mut artifacts = Vec::new();
  let mut reports = Vec::new();
  for capture in captures.values() {
    task.extend(capture.outputs.task());
    extend_unique(&mut artifacts, capture.outputs.artifacts());
    extend_unique(&mut reports, capture.outputs.reports());
  }
  if let Some(resources) = resources {
    task.extend(resources.task());
    extend_unique(&mut artifacts, resources.artifacts());
    extend_unique(&mut reports, resources.reports());
  }
  CapturedResult {
    stdout,
    outputs: CompletionOutputs::new(Map::new(), task).with_resources(artifacts, reports),
  }
}

fn extend_unique<T: Clone + PartialEq>(destination: &mut Vec<T>, values: &[T]) {
  for value in values {
    if !destination.contains(value) {
      destination.push(value.clone());
    }
  }
}

pub(super) fn action_result(
  action: Digest,
  output_bundle: Option<octa_cache_protocol::BlobDescriptor>,
  captured: &CapturedResult,
) -> ExecutorResult<ActionResultV1> {
  let artifacts = captured
    .outputs
    .artifacts()
    .iter()
    .map(|artifact| {
      Ok(CachedArtifact {
        name: artifact.name.clone(),
        path: RelativePath::new(artifact.path.clone())?,
        content_type: artifact.content_type.clone(),
      })
    })
    .collect::<Result<Vec<_>, octa_cache_protocol::CacheProtocolError>>()
    .map_err(ExecutorError::from)?;
  let reports = captured
    .outputs
    .reports()
    .iter()
    .map(|report| {
      Ok(CachedReport {
        name: report.name.clone(),
        path: RelativePath::new(report.path.clone())?,
        format: report.format.clone(),
      })
    })
    .collect::<Result<Vec<_>, octa_cache_protocol::CacheProtocolError>>()
    .map_err(ExecutorError::from)?;
  let result = ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action,
    output_bundle,
    stdout: Some(captured.stdout.clone()),
    task_outputs: captured.outputs.task().public_values().into_iter().collect(),
    artifacts,
    reports,
  };
  result.validate().map_err(ExecutorError::from)?;
  Ok(result)
}

pub(super) fn cached_result(result: ActionResultV1) -> CapturedResult {
  let mut task = TaskOutputs::default();
  for (name, value) in result.task_outputs {
    task.insert(name, value, false);
  }
  let artifacts = result
    .artifacts
    .into_iter()
    .map(|artifact| RegisteredArtifact {
      name: artifact.name,
      path: artifact.path.to_string(),
      content_type: artifact.content_type,
    })
    .collect();
  let reports = result
    .reports
    .into_iter()
    .map(|report| RegisteredReport {
      name: report.name,
      path: report.path.to_string(),
      format: report.format,
    })
    .collect();
  CapturedResult {
    stdout: result.stdout.unwrap_or_default(),
    outputs: CompletionOutputs::new(Map::new(), task).with_resources(artifacts, reports),
  }
}

pub(super) fn validate_registered_resources(plan: &TaskCachePlan, captured: &CapturedResult) -> ExecutorResult<()> {
  if captured.outputs.artifacts().is_empty() && captured.outputs.reports().is_empty() {
    return Ok(());
  }
  let workspace = dunce::canonicalize(&plan.workspace)?;
  for artifact in captured.outputs.artifacts() {
    let path = RelativePath::new(artifact.path.clone()).map_err(ExecutorError::from)?;
    validate_resource_path(plan, "artifact", &artifact.name, &path)?;
    validate_materialized_resource(&workspace, "artifact", &artifact.name, &path, false)
      .map_err(ExecutorError::from)?;
  }
  for report in captured.outputs.reports() {
    let path = RelativePath::new(report.path.clone()).map_err(ExecutorError::from)?;
    validate_resource_path(plan, "report", &report.name, &path)?;
    validate_materialized_resource(&workspace, "report", &report.name, &path, true).map_err(ExecutorError::from)?;
  }
  Ok(())
}

pub(super) fn validate_resource_contract(
  plan: &TaskCachePlan,
  artifacts: &[CachedArtifact],
  reports: &[CachedReport],
) -> ExecutorResult<()> {
  for artifact in artifacts {
    validate_resource_path(plan, "artifact", &artifact.name, &artifact.path)?;
  }
  for report in reports {
    validate_resource_path(plan, "report", &report.name, &report.path)?;
  }
  Ok(())
}

fn validate_resource_path(
  plan: &TaskCachePlan,
  kind: &'static str,
  name: &str,
  path: &RelativePath,
) -> ExecutorResult<()> {
  if !plan.outputs.iter().any(|root| path.is_within(root)) {
    return Err(ExecutorError::InvalidCacheConfiguration(format!(
      "cached {kind} '{name}' at '{path}' is not contained by files.outputs"
    )));
  }
  Ok(())
}

pub(super) fn validate_materialized_resources(
  root: &Path,
  artifacts: &[CachedArtifact],
  reports: &[CachedReport],
) -> Result<(), CacheError> {
  let root = dunce::canonicalize(root)?;
  for artifact in artifacts {
    validate_materialized_resource(&root, "artifact", &artifact.name, &artifact.path, false)?;
  }
  for report in reports {
    validate_materialized_resource(&root, "report", &report.name, &report.path, true)?;
  }
  Ok(())
}

fn validate_materialized_resource(
  root: &Path,
  kind: &'static str,
  name: &str,
  path: &RelativePath,
  require_file: bool,
) -> Result<(), CacheError> {
  let resolved = dunce::canonicalize(root.join(path.as_str())).map_err(|error| {
    CacheError::Metadata(format!(
      "cached {kind} '{name}' at '{path}' cannot be resolved: {error}"
    ))
  })?;
  let relative = resolved.strip_prefix(root).map_err(|_| {
    CacheError::Metadata(format!(
      "cached {kind} '{name}' at '{path}' resolves outside the materialization"
    ))
  })?;
  let resolved_path = RelativePath::new(relative.to_string_lossy().replace('\\', "/"))?;
  if &resolved_path != path {
    return Err(CacheError::Metadata(format!(
      "cached {kind} '{name}' at '{path}' resolves through a different path"
    )));
  }
  if require_file && !resolved.is_file() {
    return Err(CacheError::Metadata(format!(
      "cached report '{name}' at '{path}' is not a file"
    )));
  }
  Ok(())
}
