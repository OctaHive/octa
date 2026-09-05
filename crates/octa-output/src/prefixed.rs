use std::io;

use super::{ConsoleEntry, ConsolePayload, ConsoleRecord, ConsoleRenderer, ConsoleScope, ExecutionEvent};

/// Prefixes line-oriented task output while preserving the wrapped renderer's presentation.
pub struct PrefixedRenderer<R> {
  renderer: R,
}

impl<R> PrefixedRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self { renderer }
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for PrefixedRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    let ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id,
      scope: Some(scope),
      command_id,
      stream,
      payload: ConsolePayload::Line(line),
    }) = entry.record()
    else {
      return self.renderer.render(entry);
    };
    let entry = entry.with_record(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: *run_id,
      scope: Some(scope.clone()),
      command_id: command_id.clone(),
      stream: *stream,
      payload: ConsolePayload::Line(format!("[{}] {line}", scope.prefix())),
    }));
    self.renderer.render(&entry)
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleScopeAllocator, ConsoleStream};

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
      ConsolePayload::Bytes(b"prompt".to_vec()),
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
  fn delegates_capabilities_ticks_and_raw_lifecycle() {
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut renderer = PrefixedRenderer::new(RecordingRenderer::default());

    assert!(renderer.supports_raw_terminal());
    renderer.tick().unwrap();
    renderer.set_parallel(true).unwrap();
    renderer.begin_raw(&scope).unwrap();
    renderer.end_raw(&scope).unwrap();

    assert_eq!(renderer.renderer.ticks, 1);
    assert_eq!(renderer.renderer.parallel, Some(true));
    assert_eq!(renderer.renderer.raw, [("begin", scope.clone()), ("end", scope)]);
  }
}
