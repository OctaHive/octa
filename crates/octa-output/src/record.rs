use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{CliDocument, ConsoleScope};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleStream {
  Stdout,
  Stderr,
}

/// Command output is either a complete protocol line or an unchanged byte chunk.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "format", content = "data", rename_all = "snake_case")]
pub enum ConsolePayload {
  Line(String),
  Bytes(Vec<u8>),
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleStatus {
  // Declaration order is aggregation priority: performed work wins over a skip,
  // while cancellation and failure dominate both successful states.
  Skipped,
  Success,
  Cancelled,
  Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsoleLevel {
  Trace,
  Debug,
  Info,
  Warn,
  Error,
}

/// Runtime state transitions and command output produced while executing a plan.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConsoleDiagnostic {
  pub run_id: Option<u64>,
  pub scope: Option<ConsoleScope>,
  pub level: ConsoleLevel,
  pub message: String,
}

/// Structured payload carried inside a timestamped [`ConsoleEntry`].
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "category", content = "data", rename_all = "snake_case")]
pub enum ConsoleRecord {
  Execution(ExecutionEvent),
  Diagnostic(ConsoleDiagnostic),
  Document(CliDocument),
}

/// A timestamped record delivered to renderers in global output order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConsoleEntry {
  timestamp: DateTime<Utc>,
  #[serde(flatten)]
  record: ConsoleRecord,
}

impl ConsoleEntry {
  pub(crate) fn new(record: ConsoleRecord) -> Self {
    Self {
      timestamp: Utc::now(),
      record,
    }
  }

  pub fn timestamp(&self) -> &DateTime<Utc> {
    &self.timestamp
  }

  pub fn record(&self) -> &ConsoleRecord {
    &self.record
  }

  pub(crate) fn with_record(&self, record: ConsoleRecord) -> Self {
    Self {
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
    }))
    .unwrap();
    assert_eq!(value["category"], "diagnostic");
    assert_eq!(value["data"]["run_id"], 42);
    assert_eq!(value["data"]["message"], "invalid configuration");
  }
}
