use std::{
  collections::HashMap,
  io,
  time::{Duration, Instant},
};

use super::{
  ConsoleEntry, ConsolePayload, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStream, ExecutionEvent,
};

const VISIBILITY_THRESHOLD: Duration = Duration::from_secs(1);

struct PendingLine {
  entry: ConsoleEntry,
  since: Instant,
}

/// Emits a stdout status line only when it remained current for at least one second.
/// Stderr and non-output records remain live.
pub struct TimedRenderer<R> {
  renderer: R,
  pending: HashMap<ConsoleScope, PendingLine>,
}

impl<R> TimedRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self {
      renderer,
      pending: HashMap::new(),
    }
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for TimedRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    match entry.record() {
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line(_),
        ..
      }) => {
        if let Some(previous) = self.pending.insert(
          scope.clone(),
          PendingLine {
            entry: entry.clone(),
            since: Instant::now(),
          },
        ) {
          if previous.since.elapsed() >= VISIBILITY_THRESHOLD {
            self.renderer.render(&previous.entry)?;
          }
        }
        Ok(())
      },
      ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, .. }) => {
        if let Some(pending) = self.pending.remove(scope) {
          if pending.since.elapsed() >= VISIBILITY_THRESHOLD {
            self.renderer.render(&pending.entry)?;
          }
        }
        self.renderer.render(entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        payload: ConsolePayload::RawBytes(_),
        ..
      }) => {
        self.pending.remove(scope);
        self.renderer.render(entry)
      },
      _ => self.renderer.render(entry),
    }
  }

  fn tick(&mut self) -> io::Result<()> {
    let mut ready = self
      .pending
      .iter()
      .filter(|(_, pending)| pending.since.elapsed() >= VISIBILITY_THRESHOLD)
      .map(|(scope, pending)| (pending.entry.sequence(), scope.id(), scope.clone()))
      .collect::<Vec<_>>();
    ready.sort_by_key(|(sequence, scope_id, _)| (*sequence, *scope_id));
    for (_, _, scope) in ready {
      if let Some(pending) = self.pending.remove(&scope) {
        self.renderer.render(&pending.entry)?;
      }
    }
    self.renderer.tick()
  }

  fn wants_tick(&self) -> bool {
    !self.pending.is_empty()
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.pending.remove(scope);
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.end_raw(scope)
  }

  fn supports_raw_terminal(&self) -> bool {
    self.renderer.supports_raw_terminal()
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
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleRecord, ConsoleScopeAllocator, ConsoleStatus};

  #[derive(Default)]
  struct Recording(Vec<ConsoleRecord>);

  impl ConsoleRenderer for Recording {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.push(entry.record().clone());
      Ok(())
    }

    fn supports_raw_terminal(&self) -> bool {
      true
    }
  }

  fn line(scope: ConsoleScope, value: &str) -> ConsoleEntry {
    ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope),
      step_id: None,
      command_id: "command".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Line(value.to_owned()),
    }))
  }

  #[test]
  fn suppresses_short_lived_stdout_lines() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&line(scope.clone(), "one")).unwrap();
    renderer.render(&line(scope.clone(), "two")).unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope,
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();
    assert!(renderer
      .renderer
      .0
      .iter()
      .all(|record| !matches!(record, ConsoleRecord::Execution(ExecutionEvent::Output { .. }))));
  }

  #[test]
  fn tick_emits_a_line_that_remains_current_for_one_second() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    assert!(!renderer.wants_tick());
    renderer.render(&line(scope.clone(), "compiling")).unwrap();
    assert!(renderer.wants_tick());
    renderer.pending.get_mut(&scope).unwrap().since = Instant::now() - VISIBILITY_THRESHOLD;
    renderer.tick().unwrap();
    assert!(!renderer.wants_tick());
    assert!(matches!(
      renderer.renderer.0.as_slice(),
      [ConsoleRecord::Execution(ExecutionEvent::Output {
        payload: ConsolePayload::Line(line),
        ..
      })] if line == "compiling"
    ));
    assert!(renderer.pending.is_empty());
  }

  #[test]
  fn replaces_and_finishes_visible_pending_lines() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&line(scope.clone(), "one")).unwrap();
    renderer.pending.get_mut(&scope).unwrap().since = Instant::now() - VISIBILITY_THRESHOLD;
    renderer.render(&line(scope.clone(), "two")).unwrap();
    renderer.pending.get_mut(&scope).unwrap().since = Instant::now() - VISIBILITY_THRESHOLD;
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope,
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();

    let lines = renderer
      .renderer
      .0
      .iter()
      .filter_map(|record| match record {
        ConsoleRecord::Execution(ExecutionEvent::Output {
          payload: ConsolePayload::Line(line),
          ..
        }) => Some(line.as_str()),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(lines, ["one", "two"]);
  }

  #[test]
  fn raw_output_and_lifecycle_clear_pending_state() {
    let scope = ConsoleScopeAllocator::default().scope("raw");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&line(scope.clone(), "pending")).unwrap();
    let raw = ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope.clone()),
      step_id: None,
      command_id: "raw".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::RawBytes(b"raw".to_vec()),
    }));
    renderer.render(&raw).unwrap();
    assert!(renderer.pending.is_empty());

    renderer.render(&line(scope.clone(), "pending-again")).unwrap();
    renderer.begin_raw(&scope).unwrap();
    assert!(renderer.pending.is_empty());
    renderer.end_raw(&scope).unwrap();
    assert!(renderer.supports_raw_terminal());
    renderer.set_parallel(true).unwrap();
  }
}
