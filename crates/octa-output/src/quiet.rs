use std::io;

use super::{ConsoleEntry, ConsoleLevel, ConsoleRecord, ConsoleRenderer, ConsoleScope, ExecutionEvent};

/// Suppresses informational presentation while leaving task streams and warnings untouched.
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
    ) || matches!(
      entry.record(),
      ConsoleRecord::Execution(ExecutionEvent::RunStarted { .. } | ExecutionEvent::RunFinished { .. })
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

  fn wants_tick(&self) -> bool {
    self.renderer.wants_tick()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    self.renderer.set_parallel(parallel)
  }

  fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
    self.renderer.update_progress(scope, message)
  }

  fn supports_progress_updates(&self) -> bool {
    self.renderer.supports_progress_updates()
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.end_raw(scope)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleDiagnostic, ConsoleScopeAllocator};

  #[derive(Default)]
  struct Recording {
    records: Vec<ConsoleRecord>,
    ticks: usize,
    parallel: Option<bool>,
    raw: Vec<(&'static str, ConsoleScope)>,
  }

  impl ConsoleRenderer for Recording {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.records.push(entry.record().clone());
      Ok(())
    }

    fn supports_raw_terminal(&self) -> bool {
      true
    }

    fn tick(&mut self) -> io::Result<()> {
      self.ticks += 1;
      Ok(())
    }

    fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
      self.parallel = Some(parallel);
      Ok(())
    }

    fn supports_progress_updates(&self) -> bool {
      true
    }

    fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
      self.raw.push(("begin", scope.clone()));
      Ok(())
    }

    fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
      self.raw.push(("end", scope.clone()));
      Ok(())
    }
  }

  fn diagnostic(level: ConsoleLevel) -> ConsoleEntry {
    ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id: Some(1),
      scope: None,
      step_id: None,
      level,
      message: "message".to_owned(),
      location: None,
    }))
  }

  #[test]
  fn suppresses_low_priority_diagnostics_and_delegates_renderer_lifecycle() {
    let mut renderer = QuietRenderer::new(Recording::default());
    for level in [ConsoleLevel::Trace, ConsoleLevel::Debug, ConsoleLevel::Info] {
      renderer.render(&diagnostic(level)).unwrap();
    }
    for level in [ConsoleLevel::Warn, ConsoleLevel::Error] {
      renderer.render(&diagnostic(level)).unwrap();
    }
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::RunStarted {
          run_id: 1,
          command: "build".to_owned(),
        },
      )))
      .unwrap();

    let scope = ConsoleScopeAllocator::default().scope("raw");
    assert!(renderer.supports_raw_terminal());
    assert!(renderer.supports_progress_updates());
    renderer.tick().unwrap();
    renderer.set_parallel(true).unwrap();
    renderer.begin_raw(&scope).unwrap();
    renderer.end_raw(&scope).unwrap();

    assert_eq!(renderer.renderer.records.len(), 2);
    assert_eq!(renderer.renderer.ticks, 1);
    assert_eq!(renderer.renderer.parallel, Some(true));
    assert_eq!(renderer.renderer.raw, [("begin", scope.clone()), ("end", scope)]);
  }
}
