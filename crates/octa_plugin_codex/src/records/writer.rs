//! Atomic, command-scoped persistence of sanitized Codex run records.
//!
//! A [`RunRecords`] value exclusively creates one invocation directory and
//! owns every file name inside a hidden staging directory. Harness events are
//! appended to an exclusive temporary trace as they arrive. Only after all
//! three files are flushed and synchronized is that directory renamed to its
//! stable name, so readers can observe either no record set or the whole set.
//! Dropping an uncommitted value removes only those known files and the now
//! empty directory; it never recursively deletes a path derived from input.

use std::{
  collections::BTreeMap,
  io::{self, Write as _},
  path::{Path, PathBuf},
  time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{bail, Context};
use cap_std::{
  ambient_authority,
  fs::{Dir, OpenOptions},
};
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::AsyncWriteExt;

use super::normalize::{NormalizedResult, ResultPayload, SemanticOutcome};
use crate::{
  config::CodexConfig,
  contract::{RECORDS_DIRECTORY, RECORDS_STAGING_DIRECTORY, RESULT_FORMAT_VERSION},
  filesystem::{create_directory_path_no_follow, open_directory_no_follow},
  invocation::{CodexInvocation, ResultSchemaDocument},
  sanitization::{RunSanitizer, SanitizedEvent},
};

const TRACE_FORMAT_VERSION: u16 = 1;
const PROVENANCE_FORMAT_VERSION: u16 = 1;
const MAX_TRACE_BYTES: usize = 16 * 1024 * 1024;

const TRACE_FILE: &str = "trace.jsonl";
const RESULT_FILE: &str = "result.json";
const PROVENANCE_FILE: &str = "provenance.json";
const SCHEMA_FILE: &str = ".result-schema.json";
const TRACE_TEMP: &str = ".trace.jsonl.tmp";
const RESULT_TEMP: &str = ".result.json.tmp";
const PROVENANCE_TEMP: &str = ".provenance.json.tmp";
const OPEN_INVOCATION_ERROR: &str = "failed to open the Codex run-record invocation directory";
const CREATE_TRACE_ERROR: &str = "failed to create the temporary Codex trace";
const OWNED_FILES: &[&str] = &[
  TRACE_FILE,
  RESULT_FILE,
  PROVENANCE_FILE,
  SCHEMA_FILE,
  TRACE_TEMP,
  RESULT_TEMP,
  PROVENANCE_TEMP,
];

/// Exclusive owner of one invocation's temporary and stable record files.
pub(crate) struct RunRecords {
  root: Dir,
  invocation: Option<Dir>,
  staging: Option<Dir>,
  component: String,
  relative_directory: String,
  schema_path: PathBuf,
  trace: TraceWriter,
  started_unix_millis: u64,
  started: Instant,
  committed: bool,
}

/// Streaming sink that accepts only values carrying the sanitized-event type.
pub(crate) struct TraceWriter {
  file: Option<tokio::fs::File>,
  encoded_bytes: usize,
}

/// Stable relative paths created by one committed invocation.
///
/// Keeping these paths typed avoids recovering security-sensitive resource
/// declarations from the public JSON output document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RunRecordPaths {
  pub(crate) trace: String,
  pub(crate) result: String,
  pub(crate) provenance: String,
}

/// Durable records and public outputs produced by a successful commit.
#[derive(Debug)]
pub(crate) struct CommittedRunRecords {
  pub(crate) outputs: Map<String, Value>,
  pub(crate) paths: RunRecordPaths,
}

#[derive(Serialize)]
struct TraceRecord<'a> {
  format_version: u16,
  sequence: u64,
  event: &'a Value,
}

#[derive(Serialize)]
struct ResultRecord<'a> {
  format_version: u16,
  outcome: SemanticOutcome,
  #[serde(skip_serializing_if = "Option::is_none")]
  final_message: Option<&'a str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  structured_result: Option<&'a Value>,
  harness_identifiers: &'a BTreeMap<String, String>,
  usage: &'a BTreeMap<String, u64>,
}

#[derive(Serialize)]
struct ProvenanceRecord<'a> {
  format_version: u16,
  trace_format_version: u16,
  result_format_version: u16,
  plugin: SoftwareIdentity<'a>,
  codex: SoftwareIdentity<'a>,
  settings: InvocationSettings<'a>,
  prompt_digest: DigestIdentity<'a>,
  #[serde(skip_serializing_if = "Option::is_none")]
  source_revision: Option<&'a str>,
  timing: Timing,
  outcome: SemanticOutcome,
  usage: &'a BTreeMap<String, u64>,
}

#[derive(Serialize)]
struct SoftwareIdentity<'a> {
  name: &'a str,
  version: &'a str,
}

#[derive(Serialize)]
struct InvocationSettings<'a> {
  #[serde(skip_serializing_if = "Option::is_none")]
  model: Option<&'a str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  reasoning_effort: Option<&'a str>,
}

#[derive(Serialize)]
struct DigestIdentity<'a> {
  algorithm: &'a str,
  value: &'a str,
}

#[derive(Clone, Copy, Serialize)]
struct Timing {
  started_unix_millis: u64,
  finished_unix_millis: u64,
  duration_millis: u64,
}

impl RunRecords {
  /// Computes the private schema target without touching the filesystem.
  pub(crate) fn schema_path_for(working_directory: &Path, root: &str, command_id: &str) -> PathBuf {
    working_directory
      .join(root)
      .join(invocation_component(command_id))
      .join(RECORDS_STAGING_DIRECTORY)
      .join(SCHEMA_FILE)
  }

  /// Reserves a collision-resistant directory and opens the temporary trace.
  pub(crate) async fn create(working_directory: &Path, root: &str, command_id: &str) -> anyhow::Result<Self> {
    let workspace = working_directory.to_owned();
    let root_path = root.to_owned();
    let component = invocation_component(command_id);
    let relative_directory = format!("{root}/{component}");
    let schema_path = Self::schema_path_for(working_directory, root, command_id);
    let component_for_open = component.clone();
    let prepared = tokio::task::spawn_blocking(move || prepare_directory(&workspace, &root_path, &component_for_open))
      .await
      .context("Codex run-record preparation task failed")??;

    Ok(Self {
      root: prepared.root,
      invocation: Some(prepared.invocation),
      staging: Some(prepared.staging),
      component,
      relative_directory,
      schema_path,
      trace: TraceWriter {
        file: Some(tokio::fs::File::from_std(prepared.trace)),
        encoded_bytes: 0,
      },
      started_unix_millis: unix_millis(SystemTime::now()),
      started: Instant::now(),
      committed: false,
    })
  }

  /// Materializes the already-validated structured-result schema, if any.
  pub(crate) async fn materialize_schema(&mut self, schema: Option<&ResultSchemaDocument>) -> anyhow::Result<()> {
    let Some(schema) = schema else {
      return Ok(());
    };
    if schema.path() != self.schema_path {
      bail!("Codex result schema target does not belong to this invocation");
    }
    write_exclusive(self.staging().try_clone()?, SCHEMA_FILE, schema.bytes().to_vec()).await
  }

  /// Gives the command coordinator the only sink for durable harness events.
  pub(crate) fn trace(&mut self) -> &mut TraceWriter {
    &mut self.trace
  }

  /// Flushes and publishes all stable records, returning protocol outputs.
  pub(crate) async fn commit(
    mut self,
    result: NormalizedResult,
    config: &CodexConfig,
    invocation: &CodexInvocation,
    sanitizer: &RunSanitizer,
  ) -> anyhow::Result<CommittedRunRecords> {
    let elapsed = self.started.elapsed();
    let timing = Timing::from_elapsed(self.started_unix_millis, elapsed);
    let (outcome, payload, harness_identifiers, usage) = result.into_parts();
    let result_bytes = encode_result(outcome, &payload, &harness_identifiers, &usage)?;
    let codex_version = invocation.codex_version();
    let prompt_digest = invocation.prompt_identity().to_hex().to_string();
    let provenance_bytes = encode_provenance(
      outcome,
      &usage,
      config,
      &codex_version,
      &prompt_digest,
      timing,
      sanitizer,
    )?;

    self.trace.finish().await?;
    remove_if_present(self.staging(), SCHEMA_FILE)?;
    write_exclusive(self.staging().try_clone()?, RESULT_TEMP, result_bytes).await?;
    write_exclusive(self.staging().try_clone()?, PROVENANCE_TEMP, provenance_bytes).await?;
    let staging = self.staging.take().expect("record staging directory remains open");
    publish_directory(self.root.try_clone()?, self.invocation().try_clone()?, staging).await?;

    let paths = RunRecordPaths::new(&self.relative_directory);
    let outputs = build_outputs(outcome, payload, harness_identifiers, usage, &paths);
    self.committed = true;
    Ok(CommittedRunRecords { outputs, paths })
  }

  #[cfg(test)]
  pub(super) fn relative_directory(&self) -> &str {
    &self.relative_directory
  }

  #[cfg(test)]
  pub(super) fn staging_relative_directory(&self) -> String {
    format!("{}/{RECORDS_STAGING_DIRECTORY}", self.relative_directory)
  }

  fn invocation(&self) -> &Dir {
    self.invocation.as_ref().expect("invocation directory remains open")
  }

  fn staging(&self) -> &Dir {
    self.staging.as_ref().expect("record staging directory remains open")
  }
}

impl TraceWriter {
  /// Appends one versioned sanitized event without retaining the full trace.
  pub(crate) async fn append(&mut self, event: &SanitizedEvent) -> anyhow::Result<()> {
    let mut encoded = serde_json::to_vec(&TraceRecord {
      format_version: TRACE_FORMAT_VERSION,
      sequence: event.sequence(),
      event: event.value(),
    })
    .context("failed to encode a sanitized Codex trace event")?;
    encoded.push(b'\n');
    let total = self
      .encoded_bytes
      .checked_add(encoded.len())
      .filter(|total| *total <= MAX_TRACE_BYTES)
      .ok_or_else(|| anyhow::anyhow!("Codex trace exceeds {MAX_TRACE_BYTES} encoded bytes"))?;
    self
      .file
      .as_mut()
      .expect("trace remains open until record commit")
      .write_all(&encoded)
      .await
      .context("failed to write the sanitized Codex trace")?;
    self.encoded_bytes = total;
    Ok(())
  }

  async fn finish(&mut self) -> anyhow::Result<()> {
    let mut file = self.file.take().expect("trace is finalized exactly once");
    file
      .flush()
      .await
      .context("failed to flush the sanitized Codex trace")?;
    file
      .sync_all()
      .await
      .context("failed to sync the sanitized Codex trace")?;
    drop(file);
    Ok(())
  }
}

impl Drop for RunRecords {
  fn drop(&mut self) {
    if self.committed {
      return;
    }
    self.trace.file.take();
    let invocation = self.invocation.take().expect("invocation directory remains open");
    if let Some(staging) = self.staging.take() {
      remove_owned_files(&staging);
      drop(staging);
    }
    cleanup_owned_directory(&invocation, RECORDS_STAGING_DIRECTORY);
    cleanup_owned_directory(&invocation, RECORDS_DIRECTORY);
    drop(invocation);
    // The directory was exclusively created by this value. Refusing to remove
    // a non-empty directory avoids deleting files injected by another actor.
    let _ = self.root.remove_dir(&self.component);
  }
}

struct PreparedDirectory {
  root: Dir,
  invocation: Dir,
  staging: Dir,
  trace: std::fs::File,
}

fn prepare_directory(workspace: &Path, root_path: &str, component: &str) -> anyhow::Result<PreparedDirectory> {
  let workspace = Dir::open_ambient_dir(workspace, ambient_authority())
    .context("failed to open the effective task directory for Codex run records")?;
  let root = create_directory_path_no_follow(&workspace, root_path)
    .context("failed to create or safely open the Codex run-record root")?;
  match root.create_dir(component) {
    Ok(()) => {},
    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
      bail!("Codex run-record invocation directory already exists")
    },
    Err(error) => return Err(error).context("failed to create the Codex run-record invocation directory"),
  }
  let prepared = (|| {
    let invocation = open_directory_no_follow(&root, component).context(OPEN_INVOCATION_ERROR)?;
    invocation
      .create_dir(RECORDS_STAGING_DIRECTORY)
      .context("failed to create the Codex run-record staging directory")?;
    let staging = open_directory_no_follow(&invocation, RECORDS_STAGING_DIRECTORY)
      .context("failed to open the Codex run-record staging directory")?;
    let trace = create_exclusive(&staging, TRACE_TEMP).context(CREATE_TRACE_ERROR)?;
    Ok::<_, anyhow::Error>((invocation, staging, trace))
  })();
  let (invocation, staging, trace) = match prepared {
    Ok(prepared) => prepared,
    Err(error) => {
      if let Ok(invocation) = open_directory_no_follow(&root, component) {
        cleanup_owned_directory(&invocation, RECORDS_STAGING_DIRECTORY);
      }
      let _ = root.remove_dir(component);
      return Err(error);
    },
  };
  Ok(PreparedDirectory {
    root,
    invocation,
    staging,
    trace,
  })
}

fn invocation_component(command_id: &str) -> String {
  format!("run-{}", blake3::hash(command_id.as_bytes()).to_hex())
}

fn create_exclusive(directory: &Dir, name: &str) -> io::Result<std::fs::File> {
  let mut options = OpenOptions::new();
  options.write(true).create_new(true);
  directory.open_with(name, &options).map(cap_std::fs::File::into_std)
}

async fn write_exclusive(directory: Dir, name: &'static str, bytes: Vec<u8>) -> anyhow::Result<()> {
  tokio::task::spawn_blocking(move || {
    let mut file = create_exclusive(&directory, name)?;
    file.write_all(&bytes)?;
    file.sync_all()
  })
  .await
  .context("Codex run-record write task failed")?
  .with_context(|| format!("failed to write temporary Codex run record '{name}'"))
}

async fn publish_directory(root: Dir, invocation: Dir, staging: Dir) -> anyhow::Result<()> {
  tokio::task::spawn_blocking(move || {
    match invocation.symlink_metadata(RECORDS_DIRECTORY) {
      Err(error) if error.kind() == io::ErrorKind::NotFound => {},
      Ok(_) => {
        return Err(io::Error::new(
          io::ErrorKind::AlreadyExists,
          "stable run-record directory already exists",
        ))
      },
      Err(error) => return Err(error),
    }
    for final_name in [TRACE_FILE, RESULT_FILE, PROVENANCE_FILE] {
      match staging.symlink_metadata(final_name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {},
        Ok(_) => {
          return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "stable run record already exists",
          ))
        },
        Err(error) => return Err(error),
      }
    }
    for (temporary, final_name) in [
      (TRACE_TEMP, TRACE_FILE),
      (RESULT_TEMP, RESULT_FILE),
      (PROVENANCE_TEMP, PROVENANCE_FILE),
    ] {
      staging.rename(temporary, &staging, final_name)?;
    }
    sync_directory(&staging)?;
    drop(staging);
    invocation.rename(RECORDS_STAGING_DIRECTORY, &invocation, RECORDS_DIRECTORY)?;
    sync_directory(&invocation)?;
    sync_directory(&root)?;
    Ok::<_, io::Error>(())
  })
  .await
  .context("Codex run-record publication task failed")?
  .context("failed to atomically publish the Codex run-record directory")
}

fn sync_directory(directory: &Dir) -> io::Result<()> {
  // Unix requires synchronizing parent directories to make the preceding
  // renames durable across a crash. Windows does not offer the same portable
  // directory-sync operation through std/cap-std.
  #[cfg(unix)]
  directory.try_clone()?.into_std_file().sync_all()?;
  #[cfg(not(unix))]
  let _ = directory;
  Ok(())
}

fn remove_owned_files(directory: &Dir) {
  for file in OWNED_FILES {
    let _ = remove_if_present(directory, file);
  }
}

fn cleanup_owned_directory(parent: &Dir, name: &str) {
  // The harness may replace this name after the original handle was opened.
  // A no-follow reopen ensures cleanup never removes known file names through
  // a symlink or Windows reparse point into an unrelated directory.
  let Ok(directory) = open_directory_no_follow(parent, name) else {
    return;
  };
  remove_owned_files(&directory);
  drop(directory);
  // Refuse to remove a directory containing an unowned injected entry.
  let _ = parent.remove_dir(name);
}

pub(super) fn remove_if_present(directory: &Dir, name: &str) -> io::Result<()> {
  match directory.remove_file(name) {
    Ok(()) => Ok(()),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
    Err(error) => Err(error),
  }
}

fn encode_result(
  outcome: SemanticOutcome,
  payload: &ResultPayload,
  harness_identifiers: &BTreeMap<String, String>,
  usage: &BTreeMap<String, u64>,
) -> anyhow::Result<Vec<u8>> {
  let (final_message, structured_result) = match payload {
    ResultPayload::FinalMessage(message) => (Some(message.as_str()), None),
    ResultPayload::Structured(value) => (None, Some(value)),
  };
  encode_document(&ResultRecord {
    format_version: RESULT_FORMAT_VERSION,
    outcome,
    final_message,
    structured_result,
    harness_identifiers,
    usage,
  })
}

fn encode_provenance(
  outcome: SemanticOutcome,
  usage: &BTreeMap<String, u64>,
  config: &CodexConfig,
  codex_version: &str,
  prompt_digest: &str,
  timing: Timing,
  sanitizer: &RunSanitizer,
) -> anyhow::Result<Vec<u8>> {
  // These task-authored values are legitimate provenance, but they can still
  // equal a resolved secret scalar. Apply the same command-scoped redaction
  // used for harness events before any provenance bytes are serialized.
  let model = config.model.as_deref().map(|value| sanitizer.sanitize_text(value));
  let source_revision = config
    .source_revision
    .as_deref()
    .map(|value| sanitizer.sanitize_text(value));
  encode_document(&ProvenanceRecord {
    format_version: PROVENANCE_FORMAT_VERSION,
    trace_format_version: TRACE_FORMAT_VERSION,
    result_format_version: RESULT_FORMAT_VERSION,
    plugin: SoftwareIdentity {
      name: "octa_plugin_codex",
      version: env!("CARGO_PKG_VERSION"),
    },
    codex: SoftwareIdentity {
      name: "codex-cli",
      version: codex_version,
    },
    settings: InvocationSettings {
      model: model.as_deref(),
      reasoning_effort: config.reasoning_effort.map(|effort| effort.as_str()),
    },
    prompt_digest: DigestIdentity {
      algorithm: "blake3",
      value: prompt_digest,
    },
    source_revision: source_revision.as_deref(),
    timing,
    outcome,
    usage,
  })
}

fn encode_document(value: &impl Serialize) -> anyhow::Result<Vec<u8>> {
  let mut bytes = serde_json::to_vec(value).context("failed to encode a Codex run record")?;
  bytes.push(b'\n');
  Ok(bytes)
}

fn build_outputs(
  outcome: SemanticOutcome,
  payload: ResultPayload,
  harness_identifiers: BTreeMap<String, String>,
  usage: BTreeMap<String, u64>,
  paths: &RunRecordPaths,
) -> Map<String, Value> {
  let mut outputs = Map::new();
  outputs.insert("outcome".to_owned(), Value::String(outcome.as_str().to_owned()));
  match payload {
    ResultPayload::FinalMessage(message) => {
      outputs.insert("final_message".to_owned(), Value::String(message));
    },
    ResultPayload::Structured(value) => {
      outputs.insert("structured_result".to_owned(), value);
    },
  }
  outputs.insert(
    "harness_identifiers".to_owned(),
    serde_json::to_value(harness_identifiers).expect("normalized harness identifiers are serializable"),
  );
  outputs.insert(
    "usage".to_owned(),
    serde_json::to_value(usage).expect("normalized usage counters are serializable"),
  );
  outputs.insert(
    "record_paths".to_owned(),
    serde_json::json!({
      "trace": paths.trace,
      "result": paths.result,
      "provenance": paths.provenance,
    }),
  );
  outputs
}

impl RunRecordPaths {
  fn new(relative_directory: &str) -> Self {
    let relative_directory = format!("{relative_directory}/{RECORDS_DIRECTORY}");
    Self {
      trace: format!("{relative_directory}/{TRACE_FILE}"),
      result: format!("{relative_directory}/{RESULT_FILE}"),
      provenance: format!("{relative_directory}/{PROVENANCE_FILE}"),
    }
  }
}

impl Timing {
  fn from_elapsed(started_unix_millis: u64, elapsed: Duration) -> Self {
    let duration_millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    Self {
      started_unix_millis,
      finished_unix_millis: started_unix_millis.saturating_add(duration_millis),
      duration_millis,
    }
  }
}

fn unix_millis(time: SystemTime) -> u64 {
  time
    .duration_since(UNIX_EPOCH)
    .ok()
    .and_then(|duration| u64::try_from(duration.as_millis()).ok())
    .unwrap_or(0)
}
