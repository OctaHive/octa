//! Stable JSONL wire types shared by `octa-runner` and external supervisors.
//!
//! The crate deliberately contains no executor, Octafile, plugin, process, or
//! async-runtime implementation. Its SemVer version and the wire protocol
//! version are independent: incompatible wire changes require a new protocol
//! version even when released in a new crate version.

use std::{collections::BTreeMap, num::NonZeroUsize, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const RUNNER_PROTOCOL_VERSION: u16 = 1;
pub const RUNNER_EVENT_SCHEMA_VERSION: u16 = 3;
pub const RUNNER_PLUGIN_PROTOCOL_VERSION: u16 = 1;
pub const MAX_RUNNER_INPUT_FRAME_BYTES: usize = 1024 * 1024;
pub const RUNNER_INPUT_SCHEMA_V1: &str = include_str!("../schema/input-v1.schema.json");
pub const RUNNER_OUTPUT_SCHEMA_V1: &str = include_str!("../schema/output-v1.schema.json");

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerCommand {
  Start {
    protocol_version: u16,
    request_id: String,
    request: Box<RunRequest>,
  },
  Cancel {
    request_id: String,
  },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
  Succeeded,
  Failed,
  Cancelled,
}

/// Wire representation of Octa's output-suppression option.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Silence {
  None,
  All,
  Stdout,
  Stderr,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RunRequest {
  pub workspace: PathBuf,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub octafile: Option<PathBuf>,
  #[serde(default = "default_data_dir")]
  pub data_dir: PathBuf,
  #[serde(default = "default_plugins_dir")]
  pub plugins_dir: PathBuf,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub plugin_lock: Option<PathBuf>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub secrets_profile: Option<PathBuf>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub plugins: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub default_plugin: Option<String>,
  pub commands: Vec<String>,
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub variables: BTreeMap<String, String>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub arguments: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub concurrency: Option<NonZeroUsize>,
  #[serde(default)]
  pub parallel: bool,
  #[serde(default)]
  pub failfast: bool,
  #[serde(default)]
  pub dry: bool,
  #[serde(default)]
  pub force: bool,
  #[serde(default)]
  pub quiet: bool,
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
    Ok(())
  }
}

/// Output envelope. Generic payloads let the runner serialize its internal
/// event and result types without making them dependencies of this crate.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerMessage<I = String, E = Value, R = Vec<Value>> {
  Hello {
    protocol_version: u16,
    octa_version: String,
    event_schema_version: u16,
    plugin_protocol_version: u16,
  },
  Capabilities {
    octa_version: String,
    runner_protocols: Vec<u16>,
    event_schemas: Vec<u16>,
    plugin_protocols: Vec<u16>,
    octafile_versions: Vec<u8>,
    platform: String,
    features: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    build_commit: Option<String>,
  },
  Accepted {
    request_id: I,
  },
  Event {
    request_id: I,
    event: E,
  },
  Finished {
    request_id: I,
    status: RunStatus,
    results: R,
  },
  Error {
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<I>,
    message: String,
  },
}

/// Fully owned message shape for external JSON deserialization.
pub type OwnedRunnerMessage = RunnerMessage<String, Value, Vec<Value>>;

impl<I> RunnerMessage<I, (), ()> {
  pub fn accepted(request_id: I) -> Self {
    Self::Accepted { request_id }
  }

  pub fn error(request_id: Option<I>, message: impl Into<String>) -> Self {
    Self::Error {
      request_id,
      message: message.into(),
    }
  }
}

impl<I, E> RunnerMessage<I, E, ()> {
  pub fn event(request_id: I, event: E) -> Self {
    Self::Event { request_id, event }
  }
}

impl<I, R> RunnerMessage<I, (), R> {
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
  use serde_json::json;

  fn request(workspace: PathBuf) -> RunRequest {
    RunRequest {
      workspace,
      octafile: None,
      data_dir: default_data_dir(),
      plugins_dir: default_plugins_dir(),
      plugin_lock: None,
      secrets_profile: None,
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
        "protocol_version": 1,
        "octa_version": "0.3.0",
        "event_schema_version": 3,
        "plugin_protocol_version": 1
      }),
      json!({
        "type": "capabilities",
        "octa_version": "0.3.0",
        "runner_protocols": [1],
        "event_schemas": [3],
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
    let input_schema: Value = serde_json::from_str(RUNNER_INPUT_SCHEMA_V1).unwrap();
    let input = json!({
      "type": "start",
      "protocol_version": RUNNER_PROTOCOL_VERSION,
      "request_id": "job-1",
      "request": { "workspace": "/workspace", "commands": ["ci"] }
    });
    assert!(jsonschema::validator_for(&input_schema).unwrap().is_valid(&input));

    let output_schema: Value = serde_json::from_str(RUNNER_OUTPUT_SCHEMA_V1).unwrap();
    let validator = jsonschema::validator_for(&output_schema).unwrap();
    for output in [
      json!({
        "type": "hello",
        "protocol_version": 1,
        "octa_version": "0.3.0",
        "event_schema_version": 3,
        "plugin_protocol_version": 1
      }),
      json!({
        "type": "capabilities",
        "octa_version": "0.3.0",
        "runner_protocols": [1],
        "event_schemas": [3],
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
          "schema_version": 3,
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
