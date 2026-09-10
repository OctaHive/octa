use serde::{Deserialize, Serialize};

/// Workspace-relative file or directory produced by a task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegisteredArtifact {
  pub name: String,
  /// Workspace-relative path using `/` separators on every platform.
  pub path: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub content_type: Option<String>,
}

/// Workspace-relative machine-readable report produced by a task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RegisteredReport {
  pub name: String,
  /// Workspace-relative path using `/` separators on every platform.
  pub path: String,
  /// Opaque format identifier interpreted by event consumers, not Octa core.
  pub format: String,
}
