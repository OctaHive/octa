//! Structured output records and their presentation boundary for Octa.

mod buffered;
mod console;
mod document;
mod github_actions;
mod json_lines;
mod keep_order;
mod mode;
mod prefixed;
mod quiet;
mod record;
mod renderer;
mod replacing;
mod router;
mod scope;
mod spool;
mod template;
mod terminal;
mod timed;

pub use buffered::{GroupRenderer, OnErrorRenderer};
pub use console::{Console, RawConsoleSession};
pub use document::{CliDocument, SummaryItem, TaskListItem};
pub use github_actions::GithubActionsRenderer;
pub use json_lines::JsonLinesRenderer;
pub use keep_order::KeepOrderRenderer;
pub use mode::RenderMode;
pub use prefixed::PrefixedRenderer;
pub use quiet::QuietRenderer;
pub use record::{
  ConsoleDiagnostic, ConsoleEntry, ConsoleLevel, ConsolePayload, ConsoleRecord, ConsoleStatus, ConsoleStream,
  ExecutionEvent, SourceLocation,
};
pub use renderer::{ConsoleRenderer, NullRenderer};
pub use replacing::ReplacingRenderer;
pub use router::{OutputRouterConfig, OutputRouterRenderer};
pub use scope::{ConsoleScope, ConsoleScopeAllocator};
pub use template::{render_output_template, validate_output_template};
pub use terminal::{TerminalColor, TerminalRenderer};
pub use timed::TimedRenderer;
