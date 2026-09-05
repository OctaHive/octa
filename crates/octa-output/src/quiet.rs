use std::io;

use super::{ConsoleEntry, ConsoleLevel, ConsoleRecord, ConsoleRenderer, ConsoleScope};

/// Suppresses informational diagnostics while leaving task streams and warnings untouched.
pub struct QuietRenderer<R> {
  renderer: R,
}

impl<R> QuietRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self { renderer }
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for QuietRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    if matches!(
      entry.record(),
      ConsoleRecord::Diagnostic(diagnostic)
        if matches!(diagnostic.level, ConsoleLevel::Trace | ConsoleLevel::Debug | ConsoleLevel::Info)
    ) {
      return Ok(());
    }
    self.renderer.render(entry)
  }

  fn supports_raw_terminal(&self) -> bool {
    self.renderer.supports_raw_terminal()
  }

  fn tick(&mut self) -> io::Result<()> {
    self.renderer.tick()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    self.renderer.set_parallel(parallel)
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.end_raw(scope)
  }
}
