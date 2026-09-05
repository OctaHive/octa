use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{CliDocument, ConsoleScope};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

/// Runtime state transitions and command output produced while executing a plan.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecutionEvent {
  RunStarted {
    run_id: u64,
    command: String,
  },
  RunFinished {
    run_id: u64,
    command: String,
    status: ConsoleStatus,
  },
  /// Registers a task invocation in declaration order before scheduling starts.
  ScopeDeclared {
    run_id: u64,
    scope: ConsoleScope,
  },
  /// Marks the first DAG node that actually begins work for the invocation.
  ScopeStarted {
    run_id: u64,
    scope: ConsoleScope,
  },
  ScopeFinished {
    run_id: u64,
    scope: ConsoleScope,
    status: ConsoleStatus,
  },
  Output {
    run_id: u64,
    scope: Option<ConsoleScope>,
    command_id: String,
    stream: ConsoleStream,
    payload: ConsolePayload,
  },
}

/// Human-oriented diagnostic enriched with optional execution context.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConsoleDiagnostic {
  pub run_id: Option<u64>,
  pub scope: Option<ConsoleScope>,
  pub level: ConsoleLevel,
  pub message: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub location: Option<SourceLocation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceLocation {
  pub file: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub line: Option<u64>,
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
      schema_version: 1,
      sequence: 0,
      timestamp: Utc::now(),
      record,
    }
  }

  pub fn timestamp(&self) -> &DateTime<Utc> {
    &self.timestamp
  }

  pub fn schema_version(&self) -> u16 {
    self.schema_version
  }

  pub fn sequence(&self) -> u64 {
    self.sequence
  }

  pub(crate) fn assign_sequence(&mut self, sequence: u64) {
    self.sequence = sequence;
  }

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
  use super::*;
  use crate::{ConsoleScopeAllocator, TaskListItem};

  #[test]
  fn records_have_stable_category_and_payload_tags() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let value = serde_json::to_value(ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 42,
      scope: Some(scope),
      command_id: "command-1".to_owned(),
      stream: ConsoleStream::Stderr,
      payload: ConsolePayload::Line("failed".to_owned()),
    })))
    .unwrap();

    assert!(value["timestamp"].as_str().is_some());
    assert_eq!(value["schema_version"], 1);
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
    assert_eq!(entry.schema_version(), 1);
    assert_eq!(entry.sequence(), 42);
    assert!(*entry.timestamp() <= Utc::now());
  }
}
