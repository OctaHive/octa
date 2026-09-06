use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{CliDocument, ConsoleScope, ConsoleStep};

/// Version of the externally supported JSON Lines event contract.
pub const EVENT_SCHEMA_VERSION: u16 = 2;

/// JSON Schema for [`ConsoleEntry`] version 1.
pub const EVENT_SCHEMA_V1: &str = include_str!("../schema/events-v1.schema.json");
/// JSON Schema for [`ConsoleEntry`] version 2.
pub const EVENT_SCHEMA_V2: &str = include_str!("../schema/events-v2.schema.json");

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleStream {
  Stdout,
  Stderr,
}

/// Command output is either a complete protocol line or an unchanged byte chunk.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "format", content = "data", rename_all = "snake_case")]
pub enum ConsolePayload {
  Line(String),
  Bytes(#[serde(with = "base64_bytes")] Vec<u8>),
  /// Bytes belonging to an exclusive interactive terminal session.
  RawBytes(#[serde(with = "base64_bytes")] Vec<u8>),
}

mod base64_bytes {
  use base64::{engine::general_purpose::STANDARD, Engine as _};
  use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

  pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&STANDARD.encode(bytes))
  }

  pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
  where
    D: Deserializer<'de>,
  {
    STANDARD
      .decode(String::deserialize(deserializer)?)
      .map_err(D::Error::custom)
  }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleStatus {
  // Declaration order is aggregation priority: performed work wins over a skip,
  // while cancellation and failure dominate both successful states.
  Skipped,
  Success,
  Cancelled,
  Failed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleLevel {
  Trace,
  Debug,
  Info,
  Warn,
  Error,
}

/// Transient progress reported by a concrete plugin command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProgressUpdate {
  /// Human-readable description of the current activity.
  pub message: String,
  /// Completed amount, when the operation exposes a measurable position.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub current: Option<u64>,
  /// Total amount, when it is known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub total: Option<u64>,
  /// Unit for `current` and `total`, for example `files` or `bytes`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub unit: Option<String>,
}

/// Runtime state transitions and command output produced while executing a plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionEvent {
  /// Opens one executor invocation.
  RunStarted { run_id: u64, command: String },
  /// Closes a run after all task scopes have reached terminal states.
  RunFinished {
    run_id: u64,
    command: String,
    status: ConsoleStatus,
  },
  /// Registers a task invocation in declaration order before scheduling starts.
  ScopeDeclared { run_id: u64, scope: ConsoleScope },
  /// Marks the first DAG node that actually begins work for the invocation.
  ScopeStarted { run_id: u64, scope: ConsoleScope },
  /// Closes a task invocation after all of its DAG nodes have completed.
  ScopeFinished {
    run_id: u64,
    scope: ConsoleScope,
    status: ConsoleStatus,
  },
  /// Registers an executable command in plan declaration order.
  StepDeclared {
    run_id: u64,
    scope: ConsoleScope,
    step: ConsoleStep,
  },
  /// Marks evaluation of a command after scheduler capacity is acquired.
  StepStarted {
    run_id: u64,
    scope: ConsoleScope,
    step: ConsoleStep,
  },
  /// Closes a command; queued cancellation may produce this without [`Self::StepStarted`].
  StepFinished {
    run_id: u64,
    scope: ConsoleScope,
    step: ConsoleStep,
    status: ConsoleStatus,
  },
  /// Carries one streaming stdout or stderr chunk from a concrete plugin command.
  Output {
    run_id: u64,
    scope: Option<ConsoleScope>,
    /// Plan-level command identity, present when `scope` identifies its owning task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step_id: Option<u64>,
    command_id: String,
    stream: ConsoleStream,
    payload: ConsolePayload,
  },
  /// Reports transient structured progress without treating it as command output.
  Progress {
    run_id: u64,
    scope: Option<ConsoleScope>,
    /// Plan-level command identity, present when `scope` identifies its owning task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    step_id: Option<u64>,
    command_id: String,
    progress: ProgressUpdate,
  },
}

/// Human-oriented diagnostic enriched with optional execution context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConsoleDiagnostic {
  /// Run that produced the diagnostic, or `None` outside execution.
  pub run_id: Option<u64>,
  /// Task invocation that produced the diagnostic, when known.
  pub scope: Option<ConsoleScope>,
  /// Executable step within `scope`, when known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub step_id: Option<u64>,
  pub level: ConsoleLevel,
  pub message: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub location: Option<SourceLocation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceLocation {
  /// Source path reported by the plugin or configuration loader.
  pub file: String,
  /// One-based source line when available.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub line: Option<u64>,
  /// One-based source column when available.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub column: Option<u64>,
}

/// Structured payload carried inside a timestamped [`ConsoleEntry`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "category", content = "data", rename_all = "snake_case")]
pub enum ConsoleRecord {
  Execution(ExecutionEvent),
  Diagnostic(ConsoleDiagnostic),
  Document(CliDocument),
}

/// A timestamped record delivered to renderers in global output order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConsoleEntry {
  schema_version: u16,
  sequence: u64,
  timestamp: DateTime<Utc>,
  #[serde(flatten)]
  record: ConsoleRecord,
}

impl ConsoleEntry {
  pub(crate) fn new(record: ConsoleRecord) -> Self {
    Self {
      schema_version: EVENT_SCHEMA_VERSION,
      sequence: 0,
      timestamp: Utc::now(),
      record,
    }
  }

  /// Time at which the console accepted the record.
  pub fn timestamp(&self) -> &DateTime<Utc> {
    &self.timestamp
  }

  /// Version of the external event contract used by this record.
  pub fn schema_version(&self) -> u16 {
    self.schema_version
  }

  /// Monotonic position in this process's console stream.
  pub fn sequence(&self) -> u64 {
    self.sequence
  }

  pub(crate) fn assign_sequence(&mut self, sequence: u64) {
    self.sequence = sequence;
  }

  /// Structured record carried by this envelope.
  pub fn record(&self) -> &ConsoleRecord {
    &self.record
  }

  pub(crate) fn with_record(&self, record: ConsoleRecord) -> Self {
    Self {
      schema_version: self.schema_version,
      sequence: self.sequence,
      timestamp: self.timestamp,
      record,
    }
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use super::*;
  use crate::{ConsoleScopeAllocator, SummaryItem, TaskListItem};

  #[test]
  fn records_have_stable_category_and_payload_tags() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let value = serde_json::to_value(ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 42,
      scope: Some(scope),
      step_id: None,
      command_id: "command-1".to_owned(),
      stream: ConsoleStream::Stderr,
      payload: ConsolePayload::Line("failed".to_owned()),
    })))
    .unwrap();

    assert!(value["timestamp"].as_str().is_some());
    assert_eq!(value["schema_version"], EVENT_SCHEMA_VERSION);
    assert_eq!(value["sequence"], 0);
    assert_eq!(value["category"], "execution");
    assert_eq!(value["data"]["type"], "output");
    assert_eq!(value["data"]["run_id"], 42);
    assert_eq!(value["data"]["stream"], "stderr");
    assert_eq!(value["data"]["payload"]["format"], "line");

    let value = serde_json::to_value(ConsoleEntry::new(ConsoleRecord::Execution(
      ExecutionEvent::ScopeDeclared {
        run_id: 42,
        scope: ConsoleScopeAllocator::default().scope("test"),
      },
    )))
    .unwrap();
    assert_eq!(value["data"]["type"], "scope_declared");

    let value = serde_json::to_value(ConsoleRecord::Document(CliDocument::TaskList {
      tasks: vec![TaskListItem {
        name: "build".to_owned(),
        description: Some("Build project".to_owned()),
      }],
    }))
    .unwrap();
    assert_eq!(value["category"], "document");
    assert_eq!(value["data"]["type"], "task_list");
    assert_eq!(value["data"]["tasks"][0]["name"], "build");

    let value = serde_json::to_value(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id: Some(42),
      scope: None,
      step_id: None,
      level: ConsoleLevel::Error,
      message: "invalid configuration".to_owned(),
      location: None,
    }))
    .unwrap();
    assert_eq!(value["category"], "diagnostic");
    assert_eq!(value["data"]["run_id"], 42);
    assert_eq!(value["data"]["message"], "invalid configuration");
  }

  #[test]
  fn byte_payloads_round_trip_and_entry_metadata_is_accessible() {
    let payloads = [
      ConsolePayload::Bytes(vec![0, 1, 254, 255]),
      ConsolePayload::RawBytes(vec![9, 8, 7]),
    ];
    for payload in payloads {
      let json = serde_json::to_string(&payload).unwrap();
      assert_eq!(serde_json::from_str::<ConsolePayload>(&json).unwrap(), payload);
    }
    assert!(serde_json::from_str::<ConsolePayload>(r#"{"format":"bytes","data":"%%%"}"#).is_err());

    let mut entry = ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::RunStarted {
      run_id: 1,
      command: "build".to_owned(),
    }));
    entry.assign_sequence(42);
    assert_eq!(entry.schema_version(), EVENT_SCHEMA_VERSION);
    assert_eq!(entry.sequence(), 42);
    assert!(*entry.timestamp() <= Utc::now());
  }

  #[test]
  fn current_schema_validates_every_public_record_shape() {
    let schema = serde_json::from_str(EVENT_SCHEMA_V2).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let allocator = ConsoleScopeAllocator::default();
    let parent = allocator.scope("build");
    let scope = allocator.scope_with_parent_options("compile", Some(parent.id()), None, false, false);
    let step = allocator.step(&scope, "shell");
    let records = vec![
      ConsoleRecord::Execution(ExecutionEvent::RunStarted {
        run_id: 1,
        command: "build".to_owned(),
      }),
      ConsoleRecord::Execution(ExecutionEvent::RunFinished {
        run_id: 1,
        command: "build".to_owned(),
        status: ConsoleStatus::Success,
      }),
      ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared {
        run_id: 1,
        scope: scope.clone(),
      }),
      ConsoleRecord::Execution(ExecutionEvent::ScopeStarted {
        run_id: 1,
        scope: scope.clone(),
      }),
      ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
        run_id: 1,
        scope: scope.clone(),
        status: ConsoleStatus::Success,
      }),
      ConsoleRecord::Execution(ExecutionEvent::StepDeclared {
        run_id: 1,
        scope: scope.clone(),
        step: step.clone(),
      }),
      ConsoleRecord::Execution(ExecutionEvent::StepStarted {
        run_id: 1,
        scope: scope.clone(),
        step: step.clone(),
      }),
      ConsoleRecord::Execution(ExecutionEvent::StepFinished {
        run_id: 1,
        scope: scope.clone(),
        step: step.clone(),
        status: ConsoleStatus::Success,
      }),
      ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 1,
        scope: Some(scope.clone()),
        step_id: Some(step.id()),
        command_id: "plugin-command-1".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Bytes(vec![0, 1, 2]),
      }),
      ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 1,
        scope: Some(scope.clone()),
        step_id: Some(step.id()),
        command_id: "plugin-command-2".to_owned(),
        stream: ConsoleStream::Stderr,
        payload: ConsolePayload::RawBytes(vec![3, 4, 5]),
      }),
      ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 1,
        scope: None,
        step_id: None,
        command_id: "plugin-command-3".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("output".to_owned()),
      }),
      ConsoleRecord::Execution(ExecutionEvent::Progress {
        run_id: 1,
        scope: Some(scope.clone()),
        step_id: Some(step.id()),
        command_id: "plugin-command-4".to_owned(),
        progress: ProgressUpdate {
          message: "Compiling".to_owned(),
          current: Some(3),
          total: Some(10),
          unit: Some("files".to_owned()),
        },
      }),
      ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        run_id: Some(1),
        scope: Some(scope.clone()),
        step_id: Some(step.id()),
        level: ConsoleLevel::Error,
        message: "failed".to_owned(),
        location: Some(SourceLocation {
          file: "src/main.rs".to_owned(),
          line: Some(12),
          column: Some(4),
        }),
      }),
      ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        run_id: None,
        scope: None,
        step_id: None,
        level: ConsoleLevel::Info,
        message: "ready".to_owned(),
        location: None,
      }),
      ConsoleRecord::Document(CliDocument::TaskList {
        tasks: vec![TaskListItem {
          name: "build".to_owned(),
          description: None,
        }],
      }),
      ConsoleRecord::Document(CliDocument::Help {
        text: "help".to_owned(),
      }),
      ConsoleRecord::Document(CliDocument::Completion {
        text: "completion".to_owned(),
      }),
      ConsoleRecord::Document(CliDocument::Failure {
        message: "failure".to_owned(),
      }),
      ConsoleRecord::Document(CliDocument::Summary {
        tasks: vec![SummaryItem {
          name: "build".to_owned(),
          duration: Duration::from_millis(10),
        }],
        total: Duration::from_millis(10),
      }),
    ];

    for record in records {
      let value = serde_json::to_value(ConsoleEntry::new(record)).unwrap();
      assert!(validator.is_valid(&value), "event does not match v2 schema: {value}");
    }
  }

  #[test]
  fn version_one_schema_remains_frozen_and_rejects_progress() {
    let schema: serde_json::Value = serde_json::from_str(EVENT_SCHEMA_V1).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let progress = serde_json::json!({
      "schema_version": 1,
      "sequence": 0,
      "timestamp": Utc::now(),
      "category": "execution",
      "data": {
        "type": "progress",
        "run_id": 1,
        "scope": null,
        "command_id": "command-1",
        "progress": { "message": "Compiling", "current": 1, "total": 2 }
      }
    });

    assert_eq!(schema["properties"]["schema_version"]["const"], 1);
    assert!(!validator.is_valid(&progress));
  }

  #[test]
  fn hierarchy_fields_have_stable_json_names() {
    let allocator = ConsoleScopeAllocator::default();
    let parent = allocator.scope("build");
    let task = allocator.scope_with_parent_options("compile", Some(parent.id()), None, false, false);
    let step = allocator.step(&task, "shell");
    let value = serde_json::to_value(ConsoleEntry::new(ConsoleRecord::Execution(
      ExecutionEvent::StepDeclared {
        run_id: 9,
        scope: task,
        step,
      },
    )))
    .unwrap();

    assert_eq!(value["schema_version"], EVENT_SCHEMA_VERSION);
    assert_eq!(value["data"]["type"], "step_declared");
    assert_eq!(value["data"]["scope"]["parent_task_id"], parent.id());
    assert_eq!(value["data"]["step"]["id"], 0);
    assert_eq!(value["data"]["step"]["parent_task_id"], value["data"]["scope"]["id"]);
  }

  #[test]
  fn current_schema_rejects_mismatched_categories_and_unknown_fields() {
    let schema = serde_json::from_str(EVENT_SCHEMA_V2).unwrap();
    let validator = jsonschema::validator_for(&schema).unwrap();
    let mut value = serde_json::to_value(ConsoleEntry::new(ConsoleRecord::Execution(
      ExecutionEvent::RunStarted {
        run_id: 1,
        command: "build".to_owned(),
      },
    )))
    .unwrap();

    value["category"] = serde_json::json!("diagnostic");
    assert!(!validator.is_valid(&value));
    value["category"] = serde_json::json!("execution");
    value["unexpected"] = serde_json::json!(true);
    assert!(!validator.is_valid(&value));

    let unscoped_step_output = serde_json::json!({
      "schema_version": EVENT_SCHEMA_VERSION,
      "sequence": 1,
      "timestamp": Utc::now(),
      "category": "execution",
      "data": {
        "type": "output",
        "run_id": 1,
        "scope": null,
        "step_id": 1,
        "command_id": "command-1",
        "stream": "stdout",
        "payload": { "format": "line", "data": "output" }
      }
    });
    assert!(!validator.is_valid(&unscoped_step_output));
  }
}
