use std::{collections::HashMap, io};

use super::{
  spool::EntrySpool, ConsoleEntry, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStatus, ExecutionEvent,
};

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
  group_templates: Option<GroupTemplates>,
}

struct GroupTemplates {
  begin: Option<String>,
  end: Option<String>,
}

enum ScopeOutput {
  Buffered(EntrySpool),
  Live,
}

impl<R> BufferedRenderer<R> {
  fn new(renderer: R, policy: FlushPolicy) -> Self {
    Self {
      renderer,
      scopes: HashMap::new(),
      policy,
      group_templates: None,
    }
  }
}

impl<R: ConsoleRenderer> BufferedRenderer<R> {
  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    let entries = match self.scopes.insert(scope.clone(), ScopeOutput::Live) {
      Some(ScopeOutput::Buffered(entries)) => entries,
      Some(ScopeOutput::Live) => return Ok(()),
      None => EntrySpool::default(),
    };
    let mut first_error = None;
    record_error(&mut first_error, entries.render_into(&mut self.renderer));
    record_error(&mut first_error, self.renderer.begin_raw(scope));
    first_error.map_or(Ok(()), Err)
  }

  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    if let ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, status, .. }) = entry.record() {
      return match self.scopes.remove(scope) {
        Some(ScopeOutput::Buffered(entries)) if self.policy.should_flush(*status) => {
          let mut first_error = None;
          if !scope.hides_stdout() {
            if let Some(begin) = self
              .group_templates
              .as_ref()
              .and_then(|templates| templates.begin.as_deref())
            {
              record_error(
                &mut first_error,
                group_entry(entry, scope, begin).and_then(|entry| self.renderer.render(&entry)),
              );
            }
          }
          record_error(&mut first_error, entries.render_into(&mut self.renderer));
          record_error(&mut first_error, self.renderer.render(entry));
          if !scope.hides_stdout() {
            if let Some(end) = self
              .group_templates
              .as_ref()
              .and_then(|templates| templates.end.as_deref())
            {
              record_error(
                &mut first_error,
                group_entry(entry, scope, end).and_then(|entry| self.renderer.render(&entry)),
              );
            }
          }
          first_error.map_or(Ok(()), Err)
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
      self.begin_raw(scope)?;
      return self.renderer.render(entry);
    }

    match self
      .scopes
      .entry(scope.clone())
      .or_insert_with(|| ScopeOutput::Buffered(EntrySpool::default()))
    {
      ScopeOutput::Buffered(entries) => entries.push(entry.clone()),
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

  pub fn with_templates(renderer: R, begin: Option<String>, end: Option<String>, error_only: bool) -> Self {
    let mut buffered = BufferedRenderer::new(
      renderer,
      if error_only {
        FlushPolicy::OnFailure
      } else {
        FlushPolicy::Always
      },
    );
    buffered.group_templates = Some(GroupTemplates { begin, end });
    Self(buffered)
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for GroupRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    self.0.render(entry)
  }

  fn supports_raw_terminal(&self) -> bool {
    self.0.renderer.supports_raw_terminal()
  }

  fn tick(&mut self) -> io::Result<()> {
    self.0.renderer.tick()
  }

  fn wants_tick(&self) -> bool {
    self.0.renderer.wants_tick()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    self.0.renderer.set_parallel(parallel)
  }

  fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
    self.0.renderer.update_progress(scope, message)
  }

  fn supports_progress_updates(&self) -> bool {
    self.0.renderer.supports_progress_updates()
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.0.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.0.renderer.end_raw(scope)
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

  fn supports_raw_terminal(&self) -> bool {
    self.0.renderer.supports_raw_terminal()
  }

  fn tick(&mut self) -> io::Result<()> {
    self.0.renderer.tick()
  }

  fn wants_tick(&self) -> bool {
    self.0.renderer.wants_tick()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    self.0.renderer.set_parallel(parallel)
  }

  fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
    self.0.renderer.update_progress(scope, message)
  }

  fn supports_progress_updates(&self) -> bool {
    self.0.renderer.supports_progress_updates()
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.0.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.0.renderer.end_raw(scope)
  }
}

fn record_scope(record: &ConsoleRecord) -> Option<&ConsoleScope> {
  match record {
    ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::StepStarted { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::StepFinished { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::Output { scope: Some(scope), .. }) => Some(scope),
    ConsoleRecord::Diagnostic(diagnostic) => diagnostic.scope.as_ref(),
    _ => None,
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

fn record_error(first_error: &mut Option<io::Error>, result: io::Result<()>) {
  if let Err(error) = result {
    if first_error.is_none() {
      *first_error = Some(error);
    }
  }
}

fn group_entry(reference: &ConsoleEntry, scope: &ConsoleScope, template: &str) -> io::Result<ConsoleEntry> {
  let run_id = match reference.record() {
    ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { run_id, .. }) => *run_id,
    _ => 0,
  };
  let mut values = scope.template_values();
  values.insert("TASK".to_owned(), serde_json::Value::String(scope.label().to_owned()));
  values.insert("PREFIX".to_owned(), serde_json::Value::String(scope.prefix()));
  let line = crate::render_output_template(template, &values).map_err(io::Error::other)?;
  Ok(reference.with_record(ConsoleRecord::Execution(ExecutionEvent::Output {
    run_id,
    scope: Some(scope.clone()),
    step_id: None,
    command_id: "group".to_owned(),
    stream: crate::ConsoleStream::Stdout,
    payload: crate::ConsolePayload::Line(line),
  })))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleDiagnostic, ConsoleLevel, ConsolePayload, ConsoleScopeAllocator, ConsoleStream};

  #[derive(Default)]
  struct RecordingRenderer {
    records: Vec<ConsoleRecord>,
    fail_once: bool,
    ticks: usize,
    parallel: Option<bool>,
    raw: Vec<(&'static str, ConsoleScope)>,
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
      step_id: None,
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
      step_id: None,
      level: ConsoleLevel::Info,
      message: "global".to_owned(),
      location: None,
    }));
    let scoped = entry(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id: Some(7),
      scope: Some(scope.clone()),
      step_id: None,
      level: ConsoleLevel::Info,
      message: "scoped".to_owned(),
      location: None,
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
      ..RecordingRenderer::default()
    });

    renderer.render(&started(scope.clone())).unwrap();
    renderer.render(&output(scope.clone(), "output")).unwrap();
    let error = renderer.render(&finished(scope, ConsoleStatus::Success)).unwrap_err();

    assert_eq!(error.to_string(), "render failed");
    assert_eq!(renderer.0.renderer.records.len(), 3);
  }

  #[test]
  fn group_templates_respect_scope_stdout_silence() {
    let scope = ConsoleScopeAllocator::default().scope_with_options("hidden", None, true, false);
    let mut renderer = GroupRenderer::with_templates(
      RecordingRenderer::default(),
      Some("BEGIN {{.TASK}}".to_owned()),
      Some("END {{.TASK}}".to_owned()),
      false,
    );
    renderer.render(&started(scope.clone())).unwrap();
    renderer.render(&finished(scope, ConsoleStatus::Success)).unwrap();

    assert_eq!(renderer.0.renderer.records.len(), 2);
    assert!(renderer.0.renderer.records.iter().all(|record| !matches!(
      record,
      ConsoleRecord::Execution(ExecutionEvent::Output { command_id, .. }) if command_id == "group"
    )));
  }

  #[test]
  fn group_templates_can_use_runtime_task_variables() {
    let scope = ConsoleScopeAllocator::default().scope("deploy");
    scope.set_template_values(std::collections::HashMap::from([(
      "ENVIRONMENT".to_owned(),
      serde_json::Value::String("production".to_owned()),
    )]));
    let mut renderer = GroupRenderer::with_templates(
      RecordingRenderer::default(),
      Some("BEGIN {{.TASK}} {{.ENVIRONMENT}}".to_owned()),
      None,
      false,
    );
    renderer.render(&output(scope.clone(), "release")).unwrap();
    renderer.render(&finished(scope, ConsoleStatus::Success)).unwrap();

    let lines = renderer
      .0
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
    assert_eq!(lines, ["BEGIN deploy production", "release"]);
  }

  #[test]
  fn raw_output_turns_its_scope_into_live_passthrough() {
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut renderer = GroupRenderer::new(RecordingRenderer::default());
    let raw = entry(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 7,
      scope: Some(scope.clone()),
      step_id: None,
      command_id: "shell".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::RawBytes(b"prompt".to_vec()),
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
      step_id: None,
      command_id: "interactive".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::RawBytes(b"prompt".to_vec()),
    }));

    renderer.render(&started(scope.clone())).unwrap();
    renderer.render(&raw).unwrap();
    renderer.render(&output(scope.clone(), "answer")).unwrap();
    renderer.render(&finished(scope, ConsoleStatus::Success)).unwrap();

    assert_eq!(renderer.0.renderer.records.len(), 4);
  }

  #[test]
  fn wrappers_delegate_capabilities_ticks_parallel_and_raw_lifecycle() {
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut group = GroupRenderer::with_templates(RecordingRenderer::default(), None, None, true);
    assert!(group.supports_raw_terminal());
    group.tick().unwrap();
    group.set_parallel(true).unwrap();
    group.begin_raw(&scope).unwrap();
    // Starting the same raw scope twice is idempotent at the buffering layer.
    group.begin_raw(&scope).unwrap();
    group.end_raw(&scope).unwrap();
    assert_eq!(group.0.renderer.ticks, 1);
    assert_eq!(group.0.renderer.parallel, Some(true));
    assert_eq!(group.0.renderer.raw, [("begin", scope.clone()), ("end", scope.clone())]);

    let mut on_error = OnErrorRenderer::new(RecordingRenderer::default());
    assert!(on_error.supports_raw_terminal());
    on_error.tick().unwrap();
    on_error.set_parallel(false).unwrap();
    on_error.begin_raw(&scope).unwrap();
    on_error.end_raw(&scope).unwrap();
    assert_eq!(on_error.0.renderer.ticks, 1);
    assert_eq!(on_error.0.renderer.parallel, Some(false));
    assert_eq!(on_error.0.renderer.raw, [("begin", scope.clone()), ("end", scope)]);
  }
}
