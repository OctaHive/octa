//! Headless task planning and execution for Octa.
//!
//! The crate separates configuration expansion ([`ExecutionEngine`]), DAG
//! scheduling, runtime event publication, and terminal results. Presentation is
//! supplied through `octa-output`, while plugins remain behind the plugin
//! manager and terminal adapter boundaries.

mod dotenv;
mod envs;
mod error;
mod execution_engine;
mod execution_handle;
mod execution_recorder;
mod execution_result;
mod execution_run;
mod executor;
mod freshness;
mod hash_source;
mod interactive_scope_tracker;
mod output;
mod output_capture;
mod path_hash;
mod path_pattern;
mod planner;
mod plugin;
mod runtime_coordinator;
mod runtime_output;
mod source;
mod source_strategy;
mod summary;
mod task;
mod task_identity;
mod template;
mod terminal;
mod timestamp_source;
mod variable_enum;
mod vars;
mod watcher;

pub use error::ExecutorError;
pub use execution_engine::{ExecutionEngine, ExecutionRequest, PreparedExecution};
pub use execution_handle::ExecutionHandle;
pub use execution_result::{
  ExecutionConclusion, ExecutionFailure, ExecutionFailureKind, ExecutionResult, OutputReference, StepResult,
  TaskResult, TaskRole,
};
pub use runtime_coordinator::RuntimeCoordinator;
pub use summary::Summary;
pub use terminal::{RawTerminalConnector, RawTerminalInput, RawTerminalSession, UnsupportedRawTerminal};
pub use vars::{VariablePrompt, VariableResolver};
pub use watcher::{SourceWatcher, WatchTarget};

pub(crate) use planner::TaskGraphBuilder;
