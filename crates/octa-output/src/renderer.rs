use std::io;

use super::ConsoleEntry;

/// Presents structured output records in a concrete format.
pub trait ConsoleRenderer: Send + 'static {
  /// This method runs on the dedicated output thread and must not call back into `Console`.
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()>;
}

impl<R: ConsoleRenderer + ?Sized> ConsoleRenderer for Box<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    (**self).render(entry)
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
