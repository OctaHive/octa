//! Structured output records and their presentation boundary for Octa.

mod buffered;
mod console;
mod document;
mod github_actions;
mod prefixed;
mod record;
mod renderer;
mod scope;
mod terminal;

pub use buffered::{GroupRenderer, OnErrorRenderer};
pub use console::{Console, RawConsoleSession};
pub use document::{CliDocument, SummaryItem, TaskListItem};
pub use github_actions::GithubActionsRenderer;
pub use prefixed::PrefixedRenderer;
pub use record::{
  ConsoleDiagnostic, ConsoleEntry, ConsoleLevel, ConsolePayload, ConsoleRecord, ConsoleStatus, ConsoleStream,
  ExecutionEvent,
};
pub use renderer::{ConsoleRenderer, NullRenderer};
pub use scope::{ConsoleScope, ConsoleScopeAllocator};
pub use terminal::{TerminalColor, TerminalRenderer};
