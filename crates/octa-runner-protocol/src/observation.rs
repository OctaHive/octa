//! Bounded public facts, independent of model prose or private invocation content.
use serde::{Deserialize, Serialize};

/// Capability advertised only by adapters producing public command/file facts.
pub const PUBLIC_OPERATION_OBSERVATIONS_V1: &str = "public-command-file-observations.v1";
/// Greatest UTF-8 byte length of one public workspace-relative operation path.
pub const MAX_PUBLIC_OPERATION_PATH_BYTES: usize = 1024;

/// One adapter-observed operation; never an authorization or verification result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperationObservation {
  /// Opaque adapter-derived correlation digest, scoped to the enclosing command.
  pub operation_id: String,
  /// Actual reported boundary; a completion does not imply a reported start.
  pub phase: OperationPhase,
  /// Adapter observation time, retained through delayed delivery and replay.
  pub observed_at_unix_ms: i64,
  /// Allowlisted public characteristics, with no raw arguments or output.
  pub operation: ObservedOperation,
}

/// A directly observed operation boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationPhase {
  /// The adapter observed the start.
  Started,
  /// The adapter observed completion.
  Completed,
}

/// Supported categories; absence means not reported, never inferred from text.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservedOperation {
  /// A reported edit to a public workspace-relative file, with no diff or body.
  File {
    /// Portable relative path; hidden, absolute and traversing paths are excluded.
    path: String,
    /// Directly reported change category.
    change: FileChangeKind,
    /// Reported failure; absent at start or when unmeasured.
    failed: Option<bool>,
  },
  /// Command outcome without command text, arguments, environment or output.
  Command {
    /// Reported exit status, absent when unmeasured.
    exit_code: Option<i64>,
    /// Reported failure, absent when the adapter cannot establish the outcome.
    failed: Option<bool>,
  },
}

/// Directly reported file change, independent of candidate acceptance.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileChangeKind {
  /// New file.
  Created,
  /// Existing file changed.
  Modified,
  /// File removed.
  Deleted,
}

/// Conservative portable public path; never a protected/hidden or host path.
pub fn is_public_operation_path(path: &str) -> bool {
  !path.is_empty()
    && path.len() <= MAX_PUBLIC_OPERATION_PATH_BYTES
    && !path.chars().any(|c| c.is_control() || c == '\\' || c == ':')
    && path.split('/').all(|part| !part.is_empty() && !part.starts_with('.'))
    && !path.split('/').any(|part| part == "protected")
}

impl OperationObservation {
  /// Validates finite public metadata before retention or projection.
  pub fn is_valid(&self) -> bool {
    self.operation_id.len() == 64
      && self
        .operation_id
        .bytes()
        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
      && self.observed_at_unix_ms >= 0
      && match (&self.phase, &self.operation) {
        (OperationPhase::Started, ObservedOperation::Command { exit_code, failed }) => {
          exit_code.is_none() && failed.is_none()
        },
        (OperationPhase::Started, ObservedOperation::File { failed, .. }) => failed.is_none(),
        _ => true,
      }
      && match &self.operation {
        ObservedOperation::File { path, .. } => is_public_operation_path(path),
        ObservedOperation::Command { .. } => true,
      }
  }
}
