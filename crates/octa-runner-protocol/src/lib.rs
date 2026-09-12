//! Stable JSONL wire types shared by `octa-runner` and external supervisors.
//!
//! The crate deliberately contains no executor, Octafile, plugin, process, or
//! async-runtime implementation. Its SemVer version and the wire protocol
//! version are independent: incompatible wire changes require a new protocol
//! version even when released in a new crate version.

#![warn(missing_docs)]

use std::{collections::BTreeMap, num::NonZeroUsize, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use octa_cache_protocol::CacheMode;

/// Current incompatible version of the runner command envelope.
pub const RUNNER_PROTOCOL_VERSION: u16 = 2;
/// Event schema version carried inside runner event messages.
pub const RUNNER_EVENT_SCHEMA_VERSION: u16 = 4;
/// Plugin protocol version required by this runner contract.
pub const RUNNER_PLUGIN_PROTOCOL_VERSION: u16 = 1;
/// Maximum bytes accepted for one newline-delimited input command.
pub const MAX_RUNNER_INPUT_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum accepted size of a job-scoped remote-cache bearer token file.
pub const MAX_CACHE_TOKEN_FILE_BYTES: u64 = 64 * 1024;
/// Published JSON Schema for protocol-v2 input commands.
pub const RUNNER_INPUT_SCHEMA_V2: &str = include_str!("../schema/input-v2.schema.json");
/// Published JSON Schema for protocol-v2 output messages.
pub const RUNNER_OUTPUT_SCHEMA_V2: &str = include_str!("../schema/output-v2.schema.json");

/// Command sent by a supervisor over runner standard input.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerCommand {
  /// Starts the process's single execution request.
  Start {
    /// Wire version used to decode the complete command.
    protocol_version: u16,
    /// Supervisor-assigned identifier binding all later control and output.
    request_id: String,
    /// Bounded execution configuration.
    request: Box<RunRequest>,
  },
  /// Cooperatively cancels the active request with the matching identifier.
  Cancel {
    /// Identifier supplied by the preceding start command.
    request_id: String,
  },
}

/// Terminal status of the complete runner request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
  /// Every selected task succeeded or was skipped.
  Succeeded,
  /// At least one selected task failed.
  Failed,
  /// Cancellation became the authoritative terminal outcome.
  Cancelled,
}

/// Wire representation of Octa's output-suppression option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Silence {
  /// Preserve both task streams.
  None,
  /// Suppress both task streams.
  All,
  /// Suppress standard output only.
  Stdout,
  /// Suppress standard error only.
  Stderr,
}

/// Optional remote endpoint negotiated for a future HTTP-backed layer.
///
/// Version two carries the security-sensitive shape so agents never need to
/// inject bearer values into the process environment or command line. The
/// runner validates the token file before execution; transport is activated by
/// a runner capability only when the HTTP cache implementation is available.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteCacheSession {
  /// HTTPS base URL of the agent-authorized cache service.
  pub endpoint: String,
  /// Absolute path to a bounded bearer token file; its value is never serialized.
  pub token_file: PathBuf,
  /// Per-request deadline selected by the agent.
  pub request_timeout_seconds: u64,
  /// Upper bound on this job's concurrent blob transfers.
  pub max_parallel_transfers: NonZeroUsize,
}

/// Job-scoped cache configuration supplied by the agent in protocol v2.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CacheSessionSpec {
  /// Independently controls lookup and publication permission.
  pub mode: CacheMode,
  /// Server-authorized logical scope for action records.
  pub namespace: String,
  /// Absolute agent-local L1 cache directory, separate from Octa state.
  pub local_directory: PathBuf,
  /// Exact host toolchain or immutable OCI image identity used by the job.
  pub runtime: octa_cache_protocol::RuntimeIdentity,
  /// Optional remote L2 settings; usable only when the runner advertises its transport.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub remote: Option<RemoteCacheSession>,
}

impl CacheSessionSpec {
  fn validate(&self) -> Result<(), String> {
    octa_cache_protocol::validate_namespace(&self.namespace).map_err(|error| error.to_string())?;
    if !self.local_directory.is_absolute() {
      return Err("cache local_directory must be an absolute path".to_owned());
    }
    self.runtime.validate().map_err(|error| error.to_string())?;
    if let Some(remote) = &self.remote {
      if !remote.endpoint.starts_with("https://") || remote.endpoint.len() == "https://".len() {
        return Err("remote cache endpoint must use HTTPS".to_owned());
      }
      if remote.endpoint.len() > octa_cache_protocol::MAX_CACHE_STRING_BYTES
        || remote.endpoint.chars().any(char::is_control)
      {
        return Err("remote cache endpoint is not a bounded printable string".to_owned());
      }
      if !remote.token_file.is_absolute() {
        return Err("remote cache token_file must be an absolute path".to_owned());
      }
      if remote.request_timeout_seconds == 0 {
        return Err("remote cache request timeout must be greater than zero".to_owned());
      }
    }
    Ok(())
  }
}

impl Serialize for Silence {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    match self {
      Self::None => serializer.serialize_bool(false),
      Self::All => serializer.serialize_bool(true),
      Self::Stdout => serializer.serialize_str("stdout"),
      Self::Stderr => serializer.serialize_str("stderr"),
    }
  }
}

impl<'de> Deserialize<'de> for Silence {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    struct Visitor;

    impl<'de> serde::de::Visitor<'de> for Visitor {
      type Value = Silence;

      fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a boolean, 'stdout', or 'stderr'")
      }

      fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(if value { Silence::All } else { Silence::None })
      }

      fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
      where
        E: serde::de::Error,
      {
        match value {
          "stdout" => Ok(Silence::Stdout),
          "stderr" => Ok(Silence::Stderr),
          _ => Err(E::unknown_variant(value, &["stdout", "stderr"])),
        }
      }
    }

    deserializer.deserialize_any(Visitor)
  }
}

/// Complete execution request carried by a start command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
  /// Existing absolute source workspace prepared by the supervisor.
  pub workspace: PathBuf,
  /// Optional Octafile path, resolved from the workspace when relative.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub octafile: Option<PathBuf>,
  /// Per-job Octa state directory, resolved from the workspace when relative.
  #[serde(default = "default_data_dir")]
  pub data_dir: PathBuf,
  /// Directory containing plugin manifests and executables.
  #[serde(default = "default_plugins_dir")]
  pub plugins_dir: PathBuf,
  /// Optional digest lock used to verify every launched plugin.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub plugin_lock: Option<PathBuf>,
  /// Optional environment-specific logical-secret provider profile.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub secrets_profile: Option<PathBuf>,
  /// Optional task-result cache session; valid only in runner protocol v2.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub cache: Option<CacheSessionSpec>,
  /// Additional plugin names requested by the job.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub plugins: Vec<String>,
  /// Optional default plugin selected for bare task definitions.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub default_plugin: Option<String>,
  /// Non-empty root task names to execute.
  pub commands: Vec<String>,
  /// Public task variable overrides supplied by the supervisor.
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub variables: BTreeMap<String, String>,
  /// Ordered user arguments forwarded to selected tasks.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub arguments: Vec<String>,
  /// Optional maximum number of concurrently running tasks.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub concurrency: Option<NonZeroUsize>,
  /// Whether independent graph nodes may run concurrently.
  #[serde(default)]
  pub parallel: bool,
  /// Whether the first task failure cancels remaining work.
  #[serde(default)]
  pub failfast: bool,
  /// Validate and plan without executing task commands.
  #[serde(default)]
  pub dry: bool,
  /// Ignore cache hits and task-local reuse decisions.
  #[serde(default)]
  pub force: bool,
  /// Suppress Octa's ordinary informational diagnostics.
  #[serde(default)]
  pub quiet: bool,
  /// Optional task-stream suppression policy.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub silence: Option<Silence>,
}

impl RunRequest {
  /// Checks invariants that are not expressible in the JSON schema.
  pub fn validate(&self) -> Result<(), String> {
    if !self.workspace.is_absolute() {
      return Err("workspace must be an absolute path".to_owned());
    }
    if self.commands.is_empty() {
      return Err("commands must contain at least one task".to_owned());
    }
    if self.commands.iter().any(String::is_empty) {
      return Err("commands must not contain empty task names".to_owned());
    }
    if let Some(cache) = &self.cache {
      cache.validate()?;
    }
    Ok(())
  }
}

/// Output envelope. Generic payloads let the runner serialize its internal
/// event and result types without making them dependencies of this crate.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerMessage<I = String, E = Value, R = Vec<Value>> {
  /// Initial process handshake emitted before reading a command.
  Hello {
    /// Runner command protocol selected by this process.
    protocol_version: u16,
    /// Octa release version of the executable.
    octa_version: String,
    /// Version of event payloads emitted by executions.
    event_schema_version: u16,
    /// Plugin protocol expected by the embedded runtime.
    plugin_protocol_version: u16,
  },
  /// Read-only compatibility inventory emitted by the capabilities command.
  Capabilities {
    /// Octa release version of the executable.
    octa_version: String,
    /// Runner command protocol versions accepted by this executable.
    runner_protocols: Vec<u16>,
    /// Event schema versions this executable can emit.
    event_schemas: Vec<u16>,
    /// Plugin protocol versions this executable can host.
    plugin_protocols: Vec<u16>,
    /// Octafile format versions this executable can load.
    octafile_versions: Vec<u8>,
    /// Host operating-system and architecture label.
    platform: String,
    /// Individually negotiated optional behaviors.
    features: Vec<String>,
    /// Optional source revision embedded by the build pipeline.
    #[serde(skip_serializing_if = "Option::is_none")]
    build_commit: Option<String>,
  },
  /// Confirms that request validation completed and execution may emit events.
  Accepted {
    /// Identifier copied from the start command.
    request_id: I,
  },
  /// One ordered Octa event from the active execution.
  Event {
    /// Identifier copied from the start command.
    request_id: I,
    /// Versioned event payload owned by the embedding runner crate.
    event: E,
  },
  /// Terminal execution result for an accepted request.
  Finished {
    /// Identifier copied from the start command.
    request_id: I,
    /// Authoritative status for the complete request.
    status: RunStatus,
    /// Structured root-task results owned by the embedding runner crate.
    results: R,
  },
  /// Protocol, configuration, or infrastructure failure.
  Error {
    /// Request identifier when the failing input supplied a usable one.
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<I>,
    /// Human-readable diagnostic that never contains credential values.
    message: String,
  },
}

/// Fully owned message shape for external JSON deserialization.
pub type OwnedRunnerMessage = RunnerMessage<String, Value, Vec<Value>>;

impl<I> RunnerMessage<I, (), ()> {
  /// Creates an accepted message for `request_id`.
  pub fn accepted(request_id: I) -> Self {
    Self::Accepted { request_id }
  }

  /// Creates an error message optionally associated with a request.
  pub fn error(request_id: Option<I>, message: impl Into<String>) -> Self {
    Self::Error {
      request_id,
      message: message.into(),
    }
  }
}

impl<I, E> RunnerMessage<I, E, ()> {
  /// Wraps one execution event with its request identifier.
  pub fn event(request_id: I, event: E) -> Self {
    Self::Event { request_id, event }
  }
}

impl<I, R> RunnerMessage<I, (), R> {
  /// Creates the single terminal result message for an accepted request.
  pub fn finished(request_id: I, status: RunStatus, results: R) -> Self {
    Self::Finished {
      request_id,
      status,
      results,
    }
  }
}

fn default_data_dir() -> PathBuf {
  PathBuf::from(".octa")
}

fn default_plugins_dir() -> PathBuf {
  PathBuf::from("plugins")
}

#[cfg(test)]
mod tests {
  use super::*;
  use octa_cache_protocol::{Digest, PlatformArchitecture, PlatformOs, RuntimeIdentity};
  use serde_json::json;

  fn request(workspace: PathBuf) -> RunRequest {
    RunRequest {
      workspace,
      octafile: None,
      data_dir: default_data_dir(),
      plugins_dir: default_plugins_dir(),
      plugin_lock: None,
      secrets_profile: None,
      cache: None,
      plugins: Vec::new(),
      default_plugin: None,
      commands: vec!["build".to_owned()],
      variables: BTreeMap::new(),
      arguments: Vec::new(),
      concurrency: None,
      parallel: false,
      failfast: false,
      dry: false,
      force: false,
      quiet: false,
      silence: None,
    }
  }

  #[test]
  fn validates_request_invariants() {
    let mut request = request(PathBuf::from("relative"));
    assert!(request.validate().unwrap_err().contains("absolute"));

    request.workspace = std::env::current_dir().unwrap();
    request.commands.clear();
    assert!(request.validate().unwrap_err().contains("at least one"));
    request.commands.push(String::new());
    assert!(request.validate().unwrap_err().contains("empty task"));
    request.commands[0] = "build".to_owned();
    assert!(request.validate().is_ok());

    let defaults: RunRequest = serde_json::from_value(json!({
      "workspace": std::env::current_dir().unwrap(),
      "commands": ["build"]
    }))
    .unwrap();
    assert_eq!(defaults.data_dir, PathBuf::from(".octa"));
    assert_eq!(defaults.plugins_dir, PathBuf::from("plugins"));
  }

  #[test]
  fn silence_preserves_the_published_wire_shape() {
    for (value, expected) in [
      (Silence::None, json!(false)),
      (Silence::All, json!(true)),
      (Silence::Stdout, json!("stdout")),
      (Silence::Stderr, json!("stderr")),
    ] {
      assert_eq!(serde_json::to_value(value).unwrap(), expected);
      assert_eq!(serde_json::from_value::<Silence>(expected).unwrap(), value);
    }
    assert!(serde_json::from_value::<Silence>(json!("invalid")).is_err());
    assert!(serde_json::from_value::<Silence>(json!(7)).is_err());
  }

  #[test]
  fn validates_cache_session_paths_identity_and_transport_bounds() {
    let absolute = std::env::current_dir().unwrap().join("cache");
    let spec = CacheSessionSpec {
      mode: CacheMode::ReadWrite,
      namespace: "project/example".to_owned(),
      local_directory: absolute.clone(),
      runtime: RuntimeIdentity::Native {
        os: PlatformOs::Linux,
        architecture: PlatformArchitecture::Amd64,
        environment: Digest::blake3(b"toolchain"),
      },
      remote: None,
    };
    assert!(spec.validate().is_ok());

    let mut invalid = spec.clone();
    invalid.namespace.clear();
    assert!(invalid.validate().unwrap_err().contains("namespace"));
    invalid = spec.clone();
    invalid.namespace = "x".repeat(octa_cache_protocol::MAX_CACHE_STRING_BYTES + 1);
    assert!(invalid.validate().unwrap_err().contains("namespace"));
    invalid = spec.clone();
    invalid.local_directory = PathBuf::from("relative");
    assert!(invalid.validate().unwrap_err().contains("absolute"));
    invalid = spec.clone();
    invalid.runtime = RuntimeIdentity::Native {
      os: PlatformOs::Linux,
      architecture: PlatformArchitecture::Amd64,
      environment: Digest::new(octa_cache_protocol::DigestAlgorithm::Sha256, [1; 32], 1),
    };
    assert!(invalid.validate().unwrap_err().contains("blake3"));
    invalid = spec.clone();
    invalid.runtime = RuntimeIdentity::Oci {
      os: PlatformOs::Linux,
      architecture: PlatformArchitecture::Amd64,
      image: Digest::blake3(b"image"),
    };
    assert!(invalid.validate().unwrap_err().contains("sha256"));
    invalid = spec.clone();
    invalid.runtime = RuntimeIdentity::Oci {
      os: PlatformOs::Macos,
      architecture: PlatformArchitecture::Arm64,
      image: Digest::new(octa_cache_protocol::DigestAlgorithm::Sha256, [2; 32], 1),
    };
    assert!(invalid.validate().unwrap_err().contains("macOS"));

    let remote = RemoteCacheSession {
      endpoint: "http://cache.example".to_owned(),
      token_file: std::env::current_dir().unwrap().join("token"),
      request_timeout_seconds: 30,
      max_parallel_transfers: NonZeroUsize::new(4).unwrap(),
    };
    invalid = spec.clone();
    invalid.remote = Some(remote.clone());
    assert!(invalid.validate().unwrap_err().contains("HTTPS"));
    invalid.remote.as_mut().unwrap().endpoint = "https://cache.example\n".to_owned();
    assert!(invalid.validate().unwrap_err().contains("printable"));
    invalid.remote.as_mut().unwrap().endpoint = "https://cache.example".to_owned();
    invalid.remote.as_mut().unwrap().token_file = PathBuf::from("token");
    assert!(invalid.validate().unwrap_err().contains("token_file"));
    invalid.remote.as_mut().unwrap().token_file = std::env::current_dir().unwrap().join("token");
    invalid.remote.as_mut().unwrap().request_timeout_seconds = 0;
    assert!(invalid.validate().unwrap_err().contains("timeout"));
    invalid.remote.as_mut().unwrap().request_timeout_seconds = 30;
    assert!(invalid.validate().is_ok());
  }

  #[test]
  fn messages_round_trip_without_internal_octa_types() {
    let start = RunnerCommand::Start {
      protocol_version: RUNNER_PROTOCOL_VERSION,
      request_id: "job-1".to_owned(),
      request: Box::new(request(std::env::current_dir().unwrap())),
    };
    serde_json::from_value::<RunnerCommand>(serde_json::to_value(start).unwrap()).unwrap();
    let cancel = RunnerCommand::Cancel {
      request_id: "job-1".to_owned(),
    };
    serde_json::from_value::<RunnerCommand>(serde_json::to_value(cancel).unwrap()).unwrap();

    let messages = [
      json!({
        "type": "hello",
        "protocol_version": 2,
        "octa_version": "0.3.0",
        "event_schema_version": 4,
        "plugin_protocol_version": 1
      }),
      json!({
        "type": "capabilities",
        "octa_version": "0.3.0",
        "runner_protocols": [2],
        "event_schemas": [4],
        "plugin_protocols": [1],
        "octafile_versions": [1],
        "platform": "linux-x86_64",
        "features": [],
        "build_commit": "0123456"
      }),
      serde_json::to_value(RunnerMessage::accepted("job-1".to_owned())).unwrap(),
      serde_json::to_value(RunnerMessage::event("job-1".to_owned(), json!({"sequence": 0}))).unwrap(),
      serde_json::to_value(RunnerMessage::finished(
        "job-1".to_owned(),
        RunStatus::Succeeded,
        vec![json!({"command": "build"})],
      ))
      .unwrap(),
      serde_json::to_value(RunnerMessage::error(None::<String>, "failed")).unwrap(),
      serde_json::to_value(RunnerMessage::error(Some("job-1".to_owned()), "failed")).unwrap(),
    ];
    for message in messages {
      serde_json::from_value::<OwnedRunnerMessage>(message).unwrap();
    }
    assert!(serde_json::from_value::<OwnedRunnerMessage>(json!({
      "type": "accepted",
      "request_id": "job-1",
      "unknown": true
    }))
    .is_err());

    for status in [RunStatus::Succeeded, RunStatus::Failed, RunStatus::Cancelled] {
      let encoded = serde_json::to_value(status).unwrap();
      assert_eq!(serde_json::from_value::<RunStatus>(encoded).unwrap(), status);
    }
  }

  #[test]
  fn schemas_validate_public_examples() {
    let input_schema: Value = serde_json::from_str(RUNNER_INPUT_SCHEMA_V2).unwrap();
    let input = json!({
      "type": "start",
      "protocol_version": RUNNER_PROTOCOL_VERSION,
      "request_id": "job-1",
      "request": { "workspace": "/workspace", "commands": ["ci"] }
    });
    assert!(jsonschema::validator_for(&input_schema).unwrap().is_valid(&input));
    let cache_input = json!({
      "type": "start",
      "protocol_version": 2,
      "request_id": "cached-job",
      "request": {
        "workspace": "/workspace",
        "commands": ["ci"],
        "cache": {
          "mode": "read_write",
          "namespace": "project/example",
          "local_directory": "/cache",
          "runtime": {
            "kind": "native",
            "os": "linux",
            "architecture": "amd64",
            "environment": {
              "algorithm": "blake3",
              "hash": "0101010101010101010101010101010101010101010101010101010101010101",
              "size_bytes": 9
            }
          }
        }
      }
    });
    assert!(jsonschema::validator_for(&input_schema).unwrap().is_valid(&cache_input));

    let output_schema: Value = serde_json::from_str(RUNNER_OUTPUT_SCHEMA_V2).unwrap();
    let validator = jsonschema::validator_for(&output_schema).unwrap();
    for output in [
      json!({
        "type": "hello",
        "protocol_version": 2,
        "octa_version": "0.3.0",
        "event_schema_version": 4,
        "plugin_protocol_version": 1
      }),
      json!({
        "type": "capabilities",
        "octa_version": "0.3.0",
        "runner_protocols": [2],
        "event_schemas": [4],
        "plugin_protocols": [1],
        "octafile_versions": [1],
        "platform": "linux-x86_64",
        "features": []
      }),
      json!({ "type": "accepted", "request_id": "job-1" }),
      json!({
        "type": "event",
        "request_id": "job-1",
        "event": {
          "schema_version": 4,
          "sequence": 0,
          "timestamp": "2026-09-10T00:00:00Z",
          "category": "execution",
          "data": {}
        }
      }),
      json!({
        "type": "finished",
        "request_id": "job-1",
        "status": "succeeded",
        "results": []
      }),
      json!({ "type": "error", "message": "invalid request" }),
    ] {
      assert!(validator.is_valid(&output));
    }

    let report = json!({
      "name": "benchmark",
      "path": "reports/result.json",
      "format": "acme/benchmark-v2"
    });
    let report_validator = jsonschema::validator_for(&output_schema["$defs"]["report"]).unwrap();
    assert!(report_validator.is_valid(&report));
    assert!(!report_validator.is_valid(&json!({
      "name": "benchmark",
      "path": "reports/result.json",
      "format": "invalid format"
    })));
  }
}
