use std::io;

use super::{ConsoleEntry, ConsolePayload, ConsoleRecord, ConsoleRenderer, ExecutionEvent};

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
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleScopeAllocator, ConsoleStream};

  #[derive(Default)]
  struct RecordingRenderer(Vec<ConsoleRecord>);

  impl ConsoleRenderer for RecordingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.push(entry.record().clone());
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
      renderer.renderer.0,
      vec![raw.record().clone(), unscoped.record().clone()]
    );
  }
}
