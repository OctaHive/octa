//! Construction of one concrete Codex CLI invocation.
//!
//! This module resolves the validated prompt, executable, arguments, and
//! explicitly selected child environment. Process lifetime and output parsing
//! remain separate concerns.

use std::{
  collections::{BTreeMap, HashMap, HashSet},
  ffi::OsString,
  path::{Path, PathBuf},
};

use anyhow::{bail, Context};
use cap_std::{
  ambient_authority,
  fs::{Dir, OpenOptions},
};
use octa_plugin::logger::{collect_variable_redactions, redact};
use serde_json::Value;
use tokio::{fs::File, io::AsyncReadExt, process::Command};

use crate::config::{validate_prompt, CodexConfig, EnvironmentSelection, ReasoningEffort, MAX_PROMPT_BYTES};

mod executable;

pub(crate) use executable::CodexExecutable;

/// Platform values that a non-interactive Codex process may inherit on Unix.
///
/// The allowlist supports executable/tool discovery, the Codex home directory,
/// temporary files, locale handling, and operator-selected TLS roots. Every
/// other environment entry requires an explicit task mapping.
#[cfg(unix)]
const PLATFORM_ENVIRONMENT: &[&str] = &[
  "HOME",
  "PATH",
  "TMPDIR",
  "LANG",
  "LC_ALL",
  "SSL_CERT_FILE",
  "SSL_CERT_DIR",
];

/// Platform values required by native process and Codex configuration lookup
/// on Windows. Every other environment entry requires an explicit mapping.
#[cfg(windows)]
const PLATFORM_ENVIRONMENT: &[&str] = &[
  "SystemRoot",
  "WINDIR",
  "ComSpec",
  "PATH",
  "PATHEXT",
  "TEMP",
  "TMP",
  "USERPROFILE",
  "APPDATA",
  "LOCALAPPDATA",
];

#[cfg(not(any(unix, windows)))]
const PLATFORM_ENVIRONMENT: &[&str] = &["HOME", "PATH", "TMPDIR", "TEMP", "TMP"];

/// Explicit location prepared by the process layer for a structured result.
///
/// The enum makes schema activation an intentional choice. It also prevents
/// callers from appending arbitrary strings to the Codex command line.
pub(crate) enum StructuredResultTarget {
  Disabled,
  SchemaFile(PathBuf),
}

/// Resolved task context from which the allowlisted child environment is built.
pub(crate) struct EnvironmentSources<'a> {
  pub(crate) variables: &'a HashMap<String, Value>,
  pub(crate) secret_variables: &'a [String],
  pub(crate) task_environment: &'a HashMap<String, String>,
}

/// One validated, non-interactive Codex CLI invocation.
///
/// This type deliberately has no `Debug` implementation: the prompt and child
/// environment can contain credentials and must not enter diagnostic logs.
pub(crate) struct CodexInvocation {
  executable: CodexExecutable,
  arguments: Vec<OsString>,
  environment: BTreeMap<String, String>,
  prompt: LoadedPrompt,
  result_schema: Option<ResultSchemaDocument>,
}

/// JSON Schema bytes that the process layer must materialize at `path` before
/// spawning Codex.
pub(crate) struct ResultSchemaDocument {
  path: PathBuf,
  bytes: Vec<u8>,
}

struct LoadedPrompt {
  bytes: Vec<u8>,
  identity: blake3::Hash,
}

const SANDBOX_MODE: &str = "workspace-write";
const APPROVAL_POLICY: &str = "never";

impl ReasoningEffort {
  pub(crate) const fn as_str(self) -> &'static str {
    match self {
      Self::None => "none",
      Self::Minimal => "minimal",
      Self::Low => "low",
      Self::Medium => "medium",
      Self::High => "high",
      Self::Xhigh => "xhigh",
      Self::Max => "max",
    }
  }
}

impl CodexInvocation {
  /// Loads the bounded prompt and constructs the complete fixed-shape argv.
  ///
  /// `working_directory` is the effective task directory. A configured result
  /// schema requires a caller-owned file target; an absent schema forbids one.
  pub(crate) async fn load(
    executable: CodexExecutable,
    config: &CodexConfig,
    working_directory: &Path,
    structured_result: StructuredResultTarget,
    environment_sources: EnvironmentSources<'_>,
  ) -> anyhow::Result<Self> {
    let environment = build_environment(&config.environment, environment_sources)?;
    let prompt = LoadedPrompt::load(config, working_directory).await?;
    let result_schema = ResultSchemaDocument::from_config(config, structured_result)?;
    let arguments = build_arguments(config, result_schema.as_ref());
    Ok(Self {
      executable,
      arguments,
      environment,
      prompt,
      result_schema,
    })
  }

  /// Returns the exact, compatibility-checked executable selected by the
  /// operator. The process layer must use this path directly with no shell or
  /// `PATH` lookup.
  #[cfg(test)]
  pub(crate) fn executable(&self) -> &CodexExecutable {
    &self.executable
  }

  /// Returns argv entries passed directly to `Command`, without shell parsing.
  #[cfg(test)]
  pub(crate) fn arguments(&self) -> &[OsString] {
    &self.arguments
  }

  /// Returns the complete allowlisted environment for `Command::envs`.
  ///
  /// The process layer must call `Command::env_clear` before adding these
  /// entries; inheriting the plugin process environment would defeat the
  /// explicit mapping contract. Values may contain credentials and must never
  /// be logged or serialized into run records.
  #[cfg(test)]
  pub(crate) fn environment(&self) -> &BTreeMap<String, String> {
    &self.environment
  }

  /// Returns the BLAKE3 identity of the exact UTF-8 bytes sent to Codex.
  pub(crate) fn prompt_identity(&self) -> blake3::Hash {
    self.prompt.identity
  }

  /// Returns the schema document that must exist before the child is spawned.
  pub(crate) fn result_schema(&self) -> Option<&ResultSchemaDocument> {
    self.result_schema.as_ref()
  }

  /// Returns the compatibility-checked Codex CLI release for provenance.
  pub(crate) fn codex_version(&self) -> String {
    self.executable.version()
  }

  /// Builds the exact direct child command after revalidating the executable.
  pub(crate) async fn command(&self, working_directory: &Path) -> anyhow::Result<Command> {
    let mut command = self.executable.command().await?;
    command
      .args(&self.arguments)
      .env_clear()
      .envs(&self.environment)
      .current_dir(working_directory);
    Ok(command)
  }

  /// Returns the bounded prompt bytes for the command-scoped stdin writer.
  pub(crate) fn prompt_bytes(&self) -> &[u8] {
    &self.prompt.bytes
  }
}

fn build_environment(
  selection: &EnvironmentSelection,
  sources: EnvironmentSources<'_>,
) -> anyhow::Result<BTreeMap<String, String>> {
  let EnvironmentSources {
    variables,
    secret_variables,
    task_environment,
  } = sources;
  let mut result = BTreeMap::new();
  let redactions = collect_variable_redactions(variables, secret_variables);
  for name in PLATFORM_ENVIRONMENT {
    let explicitly_mapped = selection
      .public
      .keys()
      .chain(selection.secret.keys())
      .any(|candidate| candidate.eq_ignore_ascii_case(name));
    if explicitly_mapped {
      continue;
    }
    if let Some(value) = environment_value(task_environment, name)? {
      if redact(value, &redactions) != value {
        bail!("platform environment value '{name}' contains a secret and requires an explicit secret mapping");
      }
      insert_environment(&mut result, name, value)?;
    }
  }

  let secret_variables = secret_variables.iter().map(String::as_str).collect::<HashSet<_>>();
  for (environment_name, variable_name) in &selection.public {
    if secret_variables.contains(variable_name.as_str()) {
      bail!("public environment mapping '{environment_name}' references secret variable '{variable_name}'");
    }
    let value = resolve_variable(variables, variable_name)?;
    insert_environment(&mut result, environment_name, &value)?;
  }
  for (environment_name, variable_name) in &selection.secret {
    if !secret_variables.contains(variable_name.as_str()) {
      bail!("secret environment mapping '{environment_name}' references variable '{variable_name}' that is not marked secret");
    }
    let value = resolve_variable(variables, variable_name)?;
    insert_environment(&mut result, environment_name, &value)?;
  }
  Ok(result)
}

#[cfg(windows)]
fn environment_value<'a>(environment: &'a HashMap<String, String>, name: &str) -> anyhow::Result<Option<&'a str>> {
  let mut matching = environment
    .iter()
    .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name));
  let value = matching.next().map(|(_, value)| value.as_str());
  if matching.next().is_some() {
    bail!("task environment contains multiple '{name}' entries differing only by ASCII case");
  }
  Ok(value)
}

#[cfg(not(windows))]
fn environment_value<'a>(environment: &'a HashMap<String, String>, name: &str) -> anyhow::Result<Option<&'a str>> {
  Ok(environment.get(name).map(String::as_str))
}

fn resolve_variable(variables: &HashMap<String, Value>, name: &str) -> anyhow::Result<String> {
  match variables.get(name) {
    Some(Value::String(value)) => Ok(value.clone()),
    Some(value @ (Value::Bool(_) | Value::Number(_))) => Ok(value.to_string()),
    Some(Value::Null | Value::Array(_) | Value::Object(_)) => {
      bail!("environment variable source '{name}' must resolve to a scalar value")
    },
    None => bail!("environment variable source '{name}' is not defined"),
  }
}

fn insert_environment(environment: &mut BTreeMap<String, String>, name: &str, value: &str) -> anyhow::Result<()> {
  if value.contains('\0') {
    bail!("child environment value for '{name}' must not contain NUL characters");
  }
  let name = PLATFORM_ENVIRONMENT
    .iter()
    .copied()
    .find(|candidate| candidate.eq_ignore_ascii_case(name))
    .unwrap_or(name);
  environment.insert(name.to_owned(), value.to_owned());
  Ok(())
}

impl ResultSchemaDocument {
  /// Filesystem location referenced by `--output-schema`.
  pub(crate) fn path(&self) -> &Path {
    &self.path
  }

  /// Validated JSON Schema bytes to write at [`Self::path`].
  pub(crate) fn bytes(&self) -> &[u8] {
    &self.bytes
  }

  fn from_config(config: &CodexConfig, target: StructuredResultTarget) -> anyhow::Result<Option<ResultSchemaDocument>> {
    match (&config.result_schema, target) {
      (None, StructuredResultTarget::Disabled) => Ok(None),
      (Some(schema), StructuredResultTarget::SchemaFile(path)) => {
        if path.as_os_str().is_empty() {
          bail!("result schema file path must not be empty");
        }
        let bytes = serde_json::to_vec(schema).context("failed to encode the validated result schema")?;
        Ok(Some(ResultSchemaDocument { path, bytes }))
      },
      (Some(_), StructuredResultTarget::Disabled) => {
        bail!("configured result_schema requires a result schema file target")
      },
      (None, StructuredResultTarget::SchemaFile(_)) => {
        bail!("a result schema file target requires configured result_schema")
      },
    }
  }
}

impl LoadedPrompt {
  async fn load(config: &CodexConfig, working_directory: &Path) -> anyhow::Result<Self> {
    let bytes = match (&config.prompt, &config.prompt_file) {
      (Some(prompt), None) => prompt.as_bytes().to_vec(),
      (None, Some(path)) => load_prompt_file(working_directory, path).await?,
      _ => bail!("validated Codex configuration has an invalid prompt source"),
    };
    let prompt = std::str::from_utf8(&bytes).context("prompt_file must contain valid UTF-8")?;
    validate_prompt(prompt)?;
    let identity = blake3::hash(&bytes);
    Ok(Self { bytes, identity })
  }
}

async fn load_prompt_file(working_directory: &Path, relative_path: &str) -> anyhow::Result<Vec<u8>> {
  let workspace = working_directory.to_owned();
  let requested_path = relative_path.to_owned();
  let (file, size) = tokio::task::spawn_blocking(move || open_prompt_beneath(&workspace, &requested_path))
    .await
    .context("prompt_file open task failed")??;
  read_bounded_prompt(File::from_std(file), relative_path, size).await
}

/// Opens one prompt relative to an already-open workspace directory. The
/// capability path walk cannot escape through a concurrently replaced
/// ancestor, and all subsequent reads use the returned handle rather than
/// reopening a validated path by name.
fn open_prompt_beneath(working_directory: &Path, relative_path: &str) -> anyhow::Result<(std::fs::File, usize)> {
  let root = Dir::open_ambient_dir(working_directory, ambient_authority())
    .map_err(|_| anyhow::anyhow!("failed to open the effective task directory"))?;
  open_prompt_from_directory(&root, relative_path)
}

fn open_prompt_from_directory(root: &Dir, relative_path: &str) -> anyhow::Result<(std::fs::File, usize)> {
  let link_metadata = root.symlink_metadata(relative_path).map_err(|_| {
    anyhow::anyhow!("prompt_file '{relative_path}' could not be inspected safely beneath the task directory")
  })?;
  if link_metadata.file_type().is_symlink() {
    bail!("prompt_file '{relative_path}' must not be a symbolic link");
  }
  let mut options = OpenOptions::new();
  options.read(true);
  configure_prompt_open(&mut options);
  let file = root
    .open_with(relative_path, &options)
    .map_err(|_| {
      anyhow::anyhow!("prompt_file '{relative_path}' could not be opened safely beneath the task directory")
    })?
    .into_std();
  let metadata = file
    .metadata()
    .with_context(|| format!("failed to inspect opened prompt_file '{relative_path}'"))?;
  if !metadata.is_file() {
    bail!("prompt_file '{relative_path}' must be a regular file");
  }
  if metadata.len() > MAX_PROMPT_BYTES as u64 {
    bail!("prompt_file '{relative_path}' exceeds {MAX_PROMPT_BYTES} bytes");
  }
  Ok((file, metadata.len() as usize))
}

#[cfg(unix)]
fn configure_prompt_open(options: &mut OpenOptions) {
  use cap_std::fs::OpenOptionsExt;

  options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_prompt_open(options: &mut OpenOptions) {
  use cap_std::fs::OpenOptionsExt;
  use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

  options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(not(any(unix, windows)))]
fn configure_prompt_open(_options: &mut OpenOptions) {}

async fn read_bounded_prompt(
  reader: impl tokio::io::AsyncRead + Unpin,
  relative_path: &str,
  size_hint: usize,
) -> anyhow::Result<Vec<u8>> {
  let mut bytes = Vec::with_capacity(size_hint.min(MAX_PROMPT_BYTES));
  reader
    .take((MAX_PROMPT_BYTES + 1) as u64)
    .read_to_end(&mut bytes)
    .await
    .with_context(|| format!("failed to read prompt_file '{relative_path}'"))?;
  if bytes.len() > MAX_PROMPT_BYTES {
    bail!("prompt_file '{relative_path}' exceeds {MAX_PROMPT_BYTES} bytes");
  }
  Ok(bytes)
}

fn build_arguments(config: &CodexConfig, result_schema: Option<&ResultSchemaDocument>) -> Vec<OsString> {
  let mut arguments = vec![
    OsString::from("exec"),
    OsString::from("--json"),
    OsString::from(format!("--sandbox={SANDBOX_MODE}")),
    OsString::from(format!("--ask-for-approval={APPROVAL_POLICY}")),
  ];
  if let Some(model) = &config.model {
    arguments.push(OsString::from(format!("--model={model}")));
  }
  if let Some(effort) = config.reasoning_effort {
    arguments.push(OsString::from(format!(
      "--config=model_reasoning_effort=\"{}\"",
      effort.as_str()
    )));
  }
  if let Some(schema) = result_schema {
    let mut argument = OsString::from("--output-schema=");
    argument.push(schema.path());
    arguments.push(argument);
  }
  // A single dash tells `codex exec` to read the prompt from stdin. It must
  // remain the final positional argument so no prompt bytes enter argv.
  arguments.push(OsString::from("-"));
  arguments
}

#[cfg(test)]
#[path = "invocation_tests.rs"]
mod tests;
