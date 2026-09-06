use std::{
  collections::{HashMap, HashSet, VecDeque},
  io,
};

use super::{
  spool::EntrySpool, ConsoleEntry, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStream, ExecutionEvent,
};

#[derive(Default)]
struct ScopeState {
  entries: EntrySpool,
  finished: bool,
}

/// Keeps declaration order while allowing the first pending task to stream live.
pub struct KeepOrderRenderer<R> {
  renderer: R,
  order: VecDeque<ConsoleScope>,
  scopes: HashMap<ConsoleScope, ScopeState>,
  raw_scopes: HashSet<ConsoleScope>,
}

impl<R> KeepOrderRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self {
      renderer,
      order: VecDeque::new(),
      scopes: HashMap::new(),
      raw_scopes: HashSet::new(),
    }
  }
}

impl<R: ConsoleRenderer> KeepOrderRenderer<R> {
  fn activate_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    if self.raw_scopes.insert(scope.clone()) {
      if let Some(state) = self.scopes.get_mut(scope) {
        std::mem::take(&mut state.entries).render_into(&mut self.renderer)?;
      }
    }
    Ok(())
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for KeepOrderRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    if let ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { scope, .. }) = entry.record() {
      self.order.push_back(scope.clone());
      self.scopes.entry(scope.clone()).or_default();
      return self.renderer.render(entry);
    }

    let Some(scope) = record_scope(entry.record()).cloned() else {
      return self.renderer.render(entry);
    };
    if is_raw_output(entry.record()) {
      self.activate_raw(&scope)?;
    }
    let ordered_live = self.order.front() == Some(&scope);
    let live = ordered_live || self.raw_scopes.contains(&scope);
    if live {
      self.renderer.render(entry)?;
    } else {
      self
        .scopes
        .entry(scope.clone())
        .or_default()
        .entries
        .push(entry.clone())?;
    }

    let finished = matches!(
      entry.record(),
      ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { .. })
    );
    if finished {
      self.scopes.entry(scope.clone()).or_default().finished = true;
      if ordered_live {
        self.advance()?;
      }
      self.raw_scopes.remove(&scope);
    }
    Ok(())
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

  fn update_progress_bytes(
    &mut self,
    scope: &ConsoleScope,
    command_id: &str,
    stream: ConsoleStream,
    bytes: &[u8],
  ) -> io::Result<()> {
    self.renderer.update_progress_bytes(scope, command_id, stream, bytes)
  }

  fn supports_progress_updates(&self) -> bool {
    self.renderer.supports_progress_updates()
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.activate_raw(scope)?;
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.end_raw(scope)
  }
}

fn is_raw_output(record: &ConsoleRecord) -> bool {
  matches!(
    record,
    ConsoleRecord::Execution(ExecutionEvent::Output {
      payload: crate::ConsolePayload::RawBytes(_),
      ..
    })
  )
}

impl<R: ConsoleRenderer> KeepOrderRenderer<R> {
  fn advance(&mut self) -> io::Result<()> {
    if let Some(completed) = self.order.pop_front() {
      self.scopes.remove(&completed);
    }
    while let Some(scope) = self.order.front().cloned() {
      let Some(state) = self.scopes.remove(&scope) else {
        break;
      };
      let ScopeState { entries, finished } = state;
      entries.render_into(&mut self.renderer)?;
      if finished {
        self.order.pop_front();
      } else {
        self.scopes.insert(scope, ScopeState::default());
        break;
      }
    }
    Ok(())
  }
}

fn record_scope(record: &ConsoleRecord) -> Option<&ConsoleScope> {
  match record {
    ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::StepStarted { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::StepFinished { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::Output { scope: Some(scope), .. }) => Some(scope),
    ConsoleRecord::Diagnostic(diagnostic) => diagnostic.scope.as_ref(),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsolePayload, ConsoleScopeAllocator, ConsoleStatus, ConsoleStream};

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

    fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
      self.raw.push(("begin", scope.clone()));
      Ok(())
    }

    fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
      self.raw.push(("end", scope.clone()));
      Ok(())
    }
  }

  fn event(event: ExecutionEvent) -> ConsoleEntry {
    ConsoleEntry::new(ConsoleRecord::Execution(event))
  }

  fn output(scope: ConsoleScope, line: &str) -> ConsoleEntry {
    event(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope),
      step_id: None,
      command_id: line.to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Line(line.to_owned()),
    })
  }

  #[test]
  fn streams_the_first_scope_and_replays_later_scopes_in_declaration_order() {
    let allocator = ConsoleScopeAllocator::default();
    let first = allocator.scope("first");
    let second = allocator.scope("second");
    let mut renderer = KeepOrderRenderer::new(Recording::default());
    for scope in [&first, &second] {
      renderer
        .render(&event(ExecutionEvent::ScopeDeclared {
          run_id: 1,
          scope: scope.clone(),
        }))
        .unwrap();
    }
    renderer.render(&output(second.clone(), "second")).unwrap();
    renderer.render(&output(first.clone(), "first")).unwrap();
    renderer
      .render(&event(ExecutionEvent::ScopeFinished {
        run_id: 1,
        scope: second,
        status: ConsoleStatus::Success,
      }))
      .unwrap();
    renderer
      .render(&event(ExecutionEvent::ScopeFinished {
        run_id: 1,
        scope: first,
        status: ConsoleStatus::Success,
      }))
      .unwrap();

    let lines = renderer
      .renderer
      .records
      .iter()
      .filter_map(|record| match record {
        ConsoleRecord::Execution(ExecutionEvent::Output {
          payload: ConsolePayload::Line(line),
          ..
        }) => Some(line.as_str()),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(lines, ["first", "second"]);
    assert!(renderer.order.is_empty());
    assert!(renderer.scopes.is_empty());
  }

  #[test]
  fn a_later_raw_scope_bypasses_order_buffering() {
    let allocator = ConsoleScopeAllocator::default();
    let first = allocator.scope("first");
    let second = allocator.scope("interactive");
    let mut renderer = KeepOrderRenderer::new(Recording::default());
    for scope in [&first, &second] {
      renderer
        .render(&event(ExecutionEvent::ScopeDeclared {
          run_id: 1,
          scope: scope.clone(),
        }))
        .unwrap();
    }
    renderer.render(&output(second.clone(), "buffered-before-raw")).unwrap();
    let raw = event(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(second),
      step_id: None,
      command_id: "raw".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::RawBytes(b"prompt".to_vec()),
    });
    renderer.render(&raw).unwrap();

    assert!(renderer.renderer.records.iter().any(|record| record == raw.record()));
    let lines = renderer
      .renderer
      .records
      .iter()
      .filter_map(|record| match record {
        ConsoleRecord::Execution(ExecutionEvent::Output {
          payload: ConsolePayload::Line(line),
          ..
        }) => Some(line.as_str()),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(lines, ["buffered-before-raw"]);
  }

  #[test]
  fn delegates_capabilities_ticks_parallel_and_raw_lifecycle() {
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut renderer = KeepOrderRenderer::new(Recording::default());
    renderer.render(&output(scope.clone(), "buffered")).unwrap();

    assert!(renderer.supports_raw_terminal());
    renderer.tick().unwrap();
    renderer.set_parallel(true).unwrap();
    renderer.update_progress(&scope, "working").unwrap();
    renderer
      .update_progress_bytes(&scope, "command", ConsoleStream::Stdout, b"partial")
      .unwrap();
    renderer.begin_raw(&scope).unwrap();
    renderer.end_raw(&scope).unwrap();

    assert_eq!(renderer.renderer.ticks, 1);
    assert_eq!(renderer.renderer.parallel, Some(true));
    assert_eq!(renderer.renderer.raw, [("begin", scope.clone()), ("end", scope)]);
    assert!(matches!(
      renderer.renderer.records.first(),
      Some(ConsoleRecord::Execution(ExecutionEvent::Output {
        payload: ConsolePayload::Line(line),
        ..
      })) if line == "buffered"
    ));
  }
}
