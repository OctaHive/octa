use std::time::Duration;

use serde::Serialize;

/// One timing row in a CLI execution summary.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SummaryItem {
  pub name: String,
  pub duration: Duration,
}

/// One task rendered by list and search commands.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct TaskListItem {
  pub name: String,
  pub description: Option<String>,
}

/// A complete response produced by the CLI rather than by the execution engine.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CliDocument {
  TaskList { tasks: Vec<TaskListItem> },
  Help { text: String },
  Completion { text: String },
  Failure { message: String },
  Summary { tasks: Vec<SummaryItem>, total: Duration },
}
