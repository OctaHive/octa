//! Stable plugin-owned names shared across configuration and publication.
//!
//! Keeping these values together prevents task validation, record encoding,
//! and resource registration from acquiring independent format literals.

/// Directory that appears atomically after every run record is durable.
pub(crate) const RECORDS_DIRECTORY: &str = "records";
/// Hidden sibling used while the record set is incomplete.
pub(crate) const RECORDS_STAGING_DIRECTORY: &str = ".records.tmp";

/// Artifact name reserved for the sanitized harness trace.
pub(crate) const TRACE_ARTIFACT_NAME: &str = "codex-run-trace";
/// Artifact name reserved for invocation provenance.
pub(crate) const PROVENANCE_ARTIFACT_NAME: &str = "codex-run-provenance";
/// Report name reserved for the normalized terminal result.
pub(crate) const RESULT_REPORT_NAME: &str = "codex-run-result";
/// Numeric version encoded in normalized result documents.
pub(crate) const RESULT_FORMAT_VERSION: u16 = 1;
/// Generic plugin capability proving a synchronous Codex `PreToolUse` hook
/// can be bound to an operator-selected authorization helper.
pub(crate) const TOOL_AUTHORIZATION_CAPABILITY: &str = "codex.blocking-pre-tool-authorization.v1";
/// Names task-authored deliverables may not shadow.
pub(crate) const RESERVED_RESOURCE_NAMES: [&str; 3] =
  [TRACE_ARTIFACT_NAME, PROVENANCE_ARTIFACT_NAME, RESULT_REPORT_NAME];

/// Derives the report identifier from the numeric result format version.
pub(crate) fn result_report_format() -> String {
  format!("octa.codex.result.v{RESULT_FORMAT_VERSION}")
}
