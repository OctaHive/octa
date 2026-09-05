use std::io;

use super::{ConsoleEntry, ConsoleScope};

/// Presents structured output records in a concrete format.
pub trait ConsoleRenderer: Send + 'static {
  /// This method runs on the dedicated output thread and must not call back into `Console`.
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()>;

  /// Advances time-based presentation without moving rendering off the output thread.
  fn tick(&mut self) -> io::Result<()> {
    Ok(())
  }

  /// Whether this renderer currently has time-based work for [`Self::tick`].
  fn wants_tick(&self) -> bool {
    false
  }

  /// Updates an adaptive renderer after the execution plan has been built.
  fn set_parallel(&mut self, _parallel: bool) -> io::Result<()> {
    Ok(())
  }

  /// Updates presentation-only progress without creating a runtime output record.
  fn update_progress(&mut self, _scope: &ConsoleScope, _message: &str) -> io::Result<()> {
    Ok(())
  }

  /// Whether hidden stdout can affect this renderer's presentation.
  fn supports_progress_updates(&self) -> bool {
    false
  }

  /// Suspends terminal UI before an exclusive PTY session starts.
  fn begin_raw(&mut self, _scope: &ConsoleScope) -> io::Result<()> {
    Ok(())
  }

  /// Restores terminal UI after an exclusive PTY session ends.
  fn end_raw(&mut self, _scope: &ConsoleScope) -> io::Result<()> {
    Ok(())
  }

  /// Whether this renderer can preserve an interactive byte-oriented terminal session.
  fn supports_raw_terminal(&self) -> bool {
    true
  }
}

impl<R: ConsoleRenderer + ?Sized> ConsoleRenderer for Box<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    (**self).render(entry)
  }

  fn tick(&mut self) -> io::Result<()> {
    (**self).tick()
  }

  fn wants_tick(&self) -> bool {
    (**self).wants_tick()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    (**self).set_parallel(parallel)
  }

  fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
    (**self).update_progress(scope, message)
  }

  fn supports_progress_updates(&self) -> bool {
    (**self).supports_progress_updates()
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    (**self).begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    (**self).end_raw(scope)
  }

  fn supports_raw_terminal(&self) -> bool {
    (**self).supports_raw_terminal()
  }
}

/// Discards records when an executor is embedded without a presentation sink.
#[derive(Debug, Default)]
pub struct NullRenderer;

impl ConsoleRenderer for NullRenderer {
  fn render(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
    Ok(())
  }
}
