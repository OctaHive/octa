use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Workspace-relative file or directory produced by a task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegisteredArtifact {
  pub name: String,
  pub path: PathBuf,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub content_type: Option<String>,
}

/// Workspace-relative machine-readable report produced by a task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegisteredReport {
  pub name: String,
  pub path: PathBuf,
  /// Opaque format identifier interpreted by event consumers, not Octa core.
  pub format: String,
}
