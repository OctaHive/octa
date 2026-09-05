use std::{collections::HashMap, io};

use super::{ConsoleEntry, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStatus, ExecutionEvent};

#[derive(Clone, Copy)]
enum FlushPolicy {
  Always,
  OnFailure,
}

impl FlushPolicy {
  fn should_flush(self, status: ConsoleStatus) -> bool {
    matches!(self, Self::Always) || matches!(status, ConsoleStatus::Failed | ConsoleStatus::Cancelled)
  }
}

struct BufferedRenderer<R> {
  renderer: R,
  scopes: HashMap<ConsoleScope, ScopeOutput>,
  policy: FlushPolicy,
}

enum ScopeOutput {
  Buffered(Vec<ConsoleEntry>),
  Live,
}

impl<R> BufferedRenderer<R> {
  fn new(renderer: R, policy: FlushPolicy) -> Self {
    Self {
      renderer,
      scopes: HashMap::new(),
      policy,
    }
  }
}

impl<R: ConsoleRenderer> BufferedRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    if let ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, status, .. }) = entry.record() {
      return match self.scopes.remove(scope) {
        Some(ScopeOutput::Buffered(mut entries)) if self.policy.should_flush(*status) => {
          entries.push(entry.clone());
          render_all(&mut self.renderer, entries)
        },
        Some(ScopeOutput::Buffered(_)) => Ok(()),
        Some(ScopeOutput::Live) => self.renderer.render(entry),
        None if self.policy.should_flush(*status) => self.renderer.render(entry),
        None => Ok(()),
      };
    }

    let Some(scope) = record_scope(entry.record()) else {
      return self.renderer.render(entry);
    };
    // A raw stream may be interactive. Flush its buffered prefix and keep the
    // scope live because delaying or decorating later bytes could break its protocol.
    if is_raw_output(entry.record()) {
      let mut entries = match self.scopes.insert(scope.clone(), ScopeOutput::Live) {
        Some(ScopeOutput::Buffered(entries)) => entries,
        Some(ScopeOutput::Live) => return self.renderer.render(entry),
        None => Vec::new(),
      };
      entries.push(entry.clone());
      return render_all(&mut self.renderer, entries);
    }

    match self
      .scopes
      .entry(scope.clone())
      .or_insert_with(|| ScopeOutput::Buffered(Vec::new()))
    {
      ScopeOutput::Buffered(entries) => {
        entries.push(entry.clone());
        Ok(())
      },
      ScopeOutput::Live => self.renderer.render(entry),
    }
  }
}

/// Buffers each task invocation and renders it as one contiguous block.
pub struct GroupRenderer<R>(BufferedRenderer<R>);

impl<R> GroupRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self(BufferedRenderer::new(renderer, FlushPolicy::Always))
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for GroupRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    self.0.render(entry)
  }
}

/// Buffers line-oriented task output and renders only failed or cancelled invocations.
/// Unscoped records and raw interactive streams remain live.
pub struct OnErrorRenderer<R>(BufferedRenderer<R>);

impl<R> OnErrorRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self(BufferedRenderer::new(renderer, FlushPolicy::OnFailure))
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for OnErrorRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    self.0.render(entry)
  }
}

fn record_scope(record: &ConsoleRecord) -> Option<&ConsoleScope> {
  match record {
    ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::Output { scope: Some(scope), .. }) => Some(scope),
    ConsoleRecord::Diagnostic(diagnostic) => diagnostic.scope.as_ref(),
    _ => None,
  }
}

fn is_raw_output(record: &ConsoleRecord) -> bool {
  matches!(
    record,
    ConsoleRecord::Execution(ExecutionEvent::Output {
      payload: crate::ConsolePayload::Bytes(_),
      ..
    })
  )
}

fn render_all(renderer: &mut impl ConsoleRenderer, entries: Vec<ConsoleEntry>) -> io::Result<()> {
  let mut first_error = None;
  for entry in entries {
    if let Err(error) = renderer.render(&entry) {
      if first_error.is_none() {
        first_error = Some(error);
      }
    }
  }
  first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleDiagnostic, ConsoleLevel, ConsolePayload, ConsoleScopeAllocator, ConsoleStream};

  #[derive(Default)]
  struct RecordingRenderer {
    records: Vec<ConsoleRecord>,
    fail_once: bool,
  }

  impl ConsoleRenderer for RecordingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.records.push(entry.record().clone());
      if self.fail_once {
        self.fail_once = false;
        Err(io::Error::other("render failed"))
      } else {
        Ok(())
      }
    }
  }

  fn entry(record: ConsoleRecord) -> ConsoleEntry {
    ConsoleEntry::new(record)
  }

  fn started(scope: ConsoleScope) -> ConsoleEntry {
    entry(ConsoleRecord::Execution(ExecutionEvent::ScopeStarted {
      run_id: 7,
      scope,
    }))
  }

  fn declared(scope: ConsoleScope) -> ConsoleEntry {
    entry(ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared {
      run_id: 7,
      scope,
    }))
  }

  fn output(scope: ConsoleScope, value: &str) -> ConsoleEntry {
    entry(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 7,
      scope: Some(scope),
      command_id: value.to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Line(value.to_owned()),
    }))
  }

  fn finished(scope: ConsoleScope, status: ConsoleStatus) -> ConsoleEntry {
    entry(ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
      run_id: 7,
      scope,
      status,
    }))
  }

  #[test]
  fn group_renders_completed_scopes_as_contiguous_blocks() {
    let allocator = ConsoleScopeAllocator::default();
    let slow = allocator.scope("slow");
    let fast = allocator.scope("fast");
    let mut renderer = GroupRenderer::new(RecordingRenderer::default());

    renderer.render(&started(slow.clone())).unwrap();
    renderer.render(&started(fast.clone())).unwrap();
    renderer.render(&output(slow.clone(), "slow-1")).unwrap();
    renderer.render(&output(fast.clone(), "fast")).unwrap();
    renderer.render(&output(slow.clone(), "slow-2")).unwrap();
    renderer
      .render(&finished(fast.clone(), ConsoleStatus::Success))
      .unwrap();
    renderer
      .render(&finished(slow.clone(), ConsoleStatus::Success))
      .unwrap();

    let records = &renderer.0.renderer.records;
    assert!(matches!(
      &records[0],
      ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. }) if scope == &fast
    ));
    assert!(matches!(
      &records[1],
      ConsoleRecord::Execution(ExecutionEvent::Output { command_id, .. }) if command_id == "fast"
    ));
    assert!(matches!(
      &records[3],
      ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. }) if scope == &slow
    ));
    assert!(matches!(
      &records[4],
      ConsoleRecord::Execution(ExecutionEvent::Output { command_id, .. }) if command_id == "slow-1"
    ));
    assert!(matches!(
      &records[5],
      ConsoleRecord::Execution(ExecutionEvent::Output { command_id, .. }) if command_id == "slow-2"
    ));
  }

  #[test]
  fn scoped_diagnostics_are_buffered_but_unscoped_records_are_immediate() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = GroupRenderer::new(RecordingRenderer::default());
    let global = entry(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id: Some(7),
      scope: None,
      level: ConsoleLevel::Info,
      message: "global".to_owned(),
    }));
    let scoped = entry(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id: Some(7),
      scope: Some(scope.clone()),
      level: ConsoleLevel::Info,
      message: "scoped".to_owned(),
    }));

    renderer.render(&started(scope.clone())).unwrap();
    renderer.render(&scoped).unwrap();
    renderer.render(&global).unwrap();
    assert_eq!(renderer.0.renderer.records, vec![global.record().clone()]);

    renderer.render(&finished(scope, ConsoleStatus::Success)).unwrap();
    assert_eq!(renderer.0.renderer.records[2], scoped.record().clone());
  }

  #[test]
  fn scope_declarations_remain_in_plan_order() {
    let allocator = ConsoleScopeAllocator::default();
    let first = allocator.scope("first");
    let second = allocator.scope("second");
    let mut renderer = GroupRenderer::new(RecordingRenderer::default());

    renderer.render(&declared(first.clone())).unwrap();
    renderer.render(&declared(second.clone())).unwrap();

    assert_eq!(
      renderer.0.renderer.records,
      vec![declared(first).record().clone(), declared(second).record().clone()]
    );
  }

  #[test]
  fn group_attempts_every_record_and_returns_the_first_renderer_error() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = GroupRenderer::new(RecordingRenderer {
      records: Vec::new(),
      fail_once: true,
    });

    renderer.render(&started(scope.clone())).unwrap();
    renderer.render(&output(scope.clone(), "output")).unwrap();
    let error = renderer.render(&finished(scope, ConsoleStatus::Success)).unwrap_err();

    assert_eq!(error.to_string(), "render failed");
    assert_eq!(renderer.0.renderer.records.len(), 3);
  }

  #[test]
  fn raw_output_turns_its_scope_into_live_passthrough() {
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut renderer = GroupRenderer::new(RecordingRenderer::default());
    let raw = entry(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 7,
      scope: Some(scope.clone()),
      command_id: "shell".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Bytes(b"prompt".to_vec()),
    }));

    renderer.render(&declared(scope.clone())).unwrap();
    assert_eq!(renderer.0.renderer.records.len(), 1);
    renderer.render(&started(scope.clone())).unwrap();
    assert_eq!(renderer.0.renderer.records.len(), 1);

    renderer.render(&raw).unwrap();
    assert_eq!(renderer.0.renderer.records.len(), 3);

    renderer.render(&output(scope.clone(), "answer")).unwrap();
    renderer.render(&finished(scope, ConsoleStatus::Success)).unwrap();
    assert_eq!(renderer.0.renderer.records.len(), 5);
  }

  #[test]
  fn on_error_discards_successful_scopes_and_flushes_failures() {
    let allocator = ConsoleScopeAllocator::default();
    let successful = allocator.scope("successful");
    let failed = allocator.scope("failed");
    let mut renderer = OnErrorRenderer::new(RecordingRenderer::default());

    renderer.render(&started(successful.clone())).unwrap();
    renderer.render(&output(successful.clone(), "discarded")).unwrap();
    renderer.render(&finished(successful, ConsoleStatus::Success)).unwrap();
    renderer.render(&started(failed.clone())).unwrap();
    renderer.render(&output(failed.clone(), "visible")).unwrap();
    renderer.render(&finished(failed, ConsoleStatus::Failed)).unwrap();

    assert_eq!(renderer.0.renderer.records.len(), 3);
    assert!(matches!(
      &renderer.0.renderer.records[1],
      ConsoleRecord::Execution(ExecutionEvent::Output { command_id, .. }) if command_id == "visible"
    ));
  }

  #[test]
  fn on_error_flushes_cancelled_and_discards_empty_successful_scopes() {
    let allocator = ConsoleScopeAllocator::default();
    let cancelled = allocator.scope("cancelled");
    let empty = allocator.scope("empty");
    let mut renderer = OnErrorRenderer::new(RecordingRenderer::default());

    renderer.render(&started(cancelled.clone())).unwrap();
    renderer.render(&output(cancelled.clone(), "partial")).unwrap();
    renderer.render(&finished(cancelled, ConsoleStatus::Cancelled)).unwrap();
    renderer.render(&finished(empty, ConsoleStatus::Skipped)).unwrap();

    assert_eq!(renderer.0.renderer.records.len(), 3);
  }

  #[test]
  fn on_error_keeps_raw_scopes_live_even_when_they_succeed() {
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut renderer = OnErrorRenderer::new(RecordingRenderer::default());
    let raw = entry(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 7,
      scope: Some(scope.clone()),
      command_id: "interactive".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Bytes(b"prompt".to_vec()),
    }));

    renderer.render(&started(scope.clone())).unwrap();
    renderer.render(&raw).unwrap();
    renderer.render(&output(scope.clone(), "answer")).unwrap();
    renderer.render(&finished(scope, ConsoleStatus::Success)).unwrap();

    assert_eq!(renderer.0.renderer.records.len(), 4);
  }
}
