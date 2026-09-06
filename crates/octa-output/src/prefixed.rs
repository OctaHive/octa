use std::{collections::HashMap, io};

use super::{
  ConsoleEntry, ConsolePayload, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStream, ExecutionEvent,
};

/// Prefixes logical lines in streamed task output while preserving the wrapped renderer's presentation.
pub struct PrefixedRenderer<R> {
  renderer: R,
  line_states: HashMap<(u64, u64, ConsoleStream), HashMap<String, PrefixState>>,
}

struct PrefixState {
  marker: Vec<u8>,
  at_line_start: bool,
}

impl<R> PrefixedRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self {
      renderer,
      line_states: HashMap::new(),
    }
  }

  fn line_state(
    &mut self,
    run_id: u64,
    scope: &ConsoleScope,
    stream: ConsoleStream,
    command_id: &str,
  ) -> &mut PrefixState {
    let commands = self.line_states.entry((run_id, scope.id(), stream)).or_default();
    if !commands.contains_key(command_id) {
      commands.insert(
        command_id.to_owned(),
        PrefixState {
          marker: format!("[{}] ", scope.prefix()).into_bytes(),
          at_line_start: true,
        },
      );
    }
    commands.get_mut(command_id).expect("command state was inserted above")
  }

  fn reset_line(&mut self, run_id: u64, scope_id: u64, stream: ConsoleStream, command_id: &str) {
    if let Some(state) = self
      .line_states
      .get_mut(&(run_id, scope_id, stream))
      .and_then(|commands| commands.get_mut(command_id))
    {
      state.at_line_start = true;
    }
  }

  fn prefixed_bytes(state: &mut PrefixState, bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len().saturating_add(state.marker.len()));
    for byte in bytes {
      if state.at_line_start {
        output.extend_from_slice(&state.marker);
        state.at_line_start = false;
      }
      output.push(*byte);
      if *byte == b'\n' {
        state.at_line_start = true;
      }
    }
    output
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for PrefixedRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    match entry.record() {
      ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id,
        scope: Some(scope),
        step_id,
        command_id,
        stream,
        payload: ConsolePayload::Line(line),
      }) => {
        self.reset_line(*run_id, scope.id(), *stream, command_id);
        let entry = entry.with_record(ConsoleRecord::Execution(ExecutionEvent::Output {
          run_id: *run_id,
          scope: Some(scope.clone()),
          step_id: *step_id,
          command_id: command_id.clone(),
          stream: *stream,
          payload: ConsolePayload::Line(format!("[{}] {line}", scope.prefix())),
        }));
        self.renderer.render(&entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id,
        scope: Some(scope),
        step_id,
        command_id,
        stream,
        payload: ConsolePayload::Bytes(bytes),
      }) => {
        let state = self.line_state(*run_id, scope, *stream, command_id);
        let bytes = Self::prefixed_bytes(state, bytes);
        let entry = entry.with_record(ConsoleRecord::Execution(ExecutionEvent::Output {
          run_id: *run_id,
          scope: Some(scope.clone()),
          step_id: *step_id,
          command_id: command_id.clone(),
          stream: *stream,
          payload: ConsolePayload::Bytes(bytes),
        }));
        self.renderer.render(&entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { run_id, scope, .. }) => {
        self
          .line_states
          .retain(|(candidate_run, candidate_scope, _), _| candidate_run != run_id || *candidate_scope != scope.id());
        self.renderer.render(entry)
      },
      _ => self.renderer.render(entry),
    }
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
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.end_raw(scope)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ConsoleScopeAllocator;

  #[derive(Default)]
  struct RecordingRenderer {
    records: Vec<ConsoleRecord>,
    ticks: usize,
    parallel: Option<bool>,
    raw: Vec<(&'static str, ConsoleScope)>,
  }

  impl ConsoleRenderer for RecordingRenderer {
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

  fn output(scope: Option<crate::ConsoleScope>, payload: ConsolePayload) -> ConsoleEntry {
    ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 7,
      scope,
      step_id: None,
      command_id: "command-1".to_owned(),
      stream: ConsoleStream::Stdout,
      payload,
    }))
  }

  #[test]
  fn prefixes_lines_with_the_task_name_or_custom_label() {
    let allocator = ConsoleScopeAllocator::default();
    let default = allocator.scope("build");
    let custom = allocator.scope_with_prefix("deploy", Some("production".to_owned()));
    let mut renderer = PrefixedRenderer::new(RecordingRenderer::default());

    renderer
      .render(&output(Some(default), ConsolePayload::Line("compiled".to_owned())))
      .unwrap();
    renderer
      .render(&output(Some(custom), ConsolePayload::Line("released".to_owned())))
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
    assert_eq!(lines, ["[build] compiled", "[production] released"]);
  }

  #[test]
  fn preserves_unscoped_and_raw_output() {
    let allocator = ConsoleScopeAllocator::default();
    let raw = output(
      Some(allocator.scope("interactive")),
      ConsolePayload::RawBytes(b"prompt".to_vec()),
    );
    let unscoped = output(None, ConsolePayload::Line("global".to_owned()));
    let mut renderer = PrefixedRenderer::new(RecordingRenderer::default());

    renderer.render(&raw).unwrap();
    renderer.render(&unscoped).unwrap();

    assert_eq!(
      renderer.renderer.records,
      vec![raw.record().clone(), unscoped.record().clone()]
    );
  }

  #[test]
  fn prefixes_streamed_bytes_without_waiting_for_newlines() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = PrefixedRenderer::new(RecordingRenderer::default());

    for bytes in [b"com".as_slice(), b"pile\nnext".as_slice(), b" line\n".as_slice()] {
      renderer
        .render(&output(Some(scope.clone()), ConsolePayload::Bytes(bytes.to_vec())))
        .unwrap();
    }
    renderer
      .render(&output(Some(scope.clone()), ConsolePayload::Line("legacy".to_owned())))
      .unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 7,
          scope,
          status: crate::ConsoleStatus::Success,
        },
      )))
      .unwrap();

    let bytes = renderer
      .renderer
      .records
      .iter()
      .filter_map(|record| match record {
        ConsoleRecord::Execution(ExecutionEvent::Output {
          payload: ConsolePayload::Bytes(bytes),
          ..
        }) => Some(bytes.as_slice()),
        _ => None,
      })
      .flatten()
      .copied()
      .collect::<Vec<_>>();
    assert_eq!(bytes, b"[build] compile\n[build] next line\n");
    assert!(renderer.line_states.is_empty());
  }

  #[test]
  fn delegates_capabilities_ticks_and_raw_lifecycle() {
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut renderer = PrefixedRenderer::new(RecordingRenderer::default());

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
  }
}
