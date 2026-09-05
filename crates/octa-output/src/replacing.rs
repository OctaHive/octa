use std::{collections::HashMap, io};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::{
  ConsoleEntry, ConsolePayload, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStatus, ConsoleStream,
  ExecutionEvent,
};

/// Maintains one independently updating progress row per running task.
pub struct ReplacingRenderer<R> {
  renderer: R,
  progress: MultiProgress,
  bars: HashMap<ConsoleScope, ProgressBar>,
  raw_scope: Option<ConsoleScope>,
}

impl<R> ReplacingRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self::with_draw_target(renderer, ProgressDrawTarget::stderr())
  }

  fn with_draw_target(renderer: R, target: ProgressDrawTarget) -> Self {
    Self {
      renderer,
      progress: MultiProgress::with_draw_target(target),
      bars: HashMap::new(),
      raw_scope: None,
    }
  }

  fn bar(&mut self, scope: &ConsoleScope) -> ProgressBar {
    if let Some(bar) = self.bars.get(scope) {
      bar.set_prefix(scope.prefix());
      return bar.clone();
    }
    let bar = ProgressBar::new_spinner();
    bar.set_style(
      ProgressStyle::with_template("{spinner:.cyan} [{prefix}] {wide_msg} [{elapsed_precise}]")
        .expect("static replacing progress template is valid")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    bar.set_prefix(scope.prefix());
    bar.set_message("waiting");
    let bar = self.progress.add(bar);
    self.bars.insert(scope.clone(), bar.clone());
    bar
  }

  fn finish_scope(&mut self, scope: &ConsoleScope, status: ConsoleStatus) {
    if let Some(bar) = self.bars.remove(scope) {
      let (symbol, label) = match status {
        ConsoleStatus::Success => ("✓", "done"),
        ConsoleStatus::Skipped => ("−", "skipped"),
        ConsoleStatus::Cancelled => ("!", "cancelled"),
        ConsoleStatus::Failed => ("✗", "failed"),
      };
      let message = bar.message();
      bar.set_style(
        ProgressStyle::with_template("{prefix} {wide_msg} [{elapsed_precise}]")
          .expect("static completed progress template is valid"),
      );
      bar.set_prefix(format!("{symbol} [{}]", scope.prefix()));
      bar.set_message(if matches!(message.as_str(), "waiting" | "running") {
        label.to_owned()
      } else {
        format!("{message} — {label}")
      });
      bar.finish();
    }
    if self.raw_scope.as_ref() == Some(scope) {
      self.raw_scope = None;
    }
  }

  fn activate_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    if let Some(bar) = self.bars.remove(scope) {
      bar.finish_and_clear();
    }
    self.progress.clear()?;
    self.raw_scope = Some(scope.clone());
    Ok(())
  }

  fn render_around_progress(&mut self, entry: &ConsoleEntry) -> io::Result<()>
  where
    R: ConsoleRenderer,
  {
    let renderer = &mut self.renderer;
    self.progress.suspend(|| renderer.render(entry))
  }

  pub(crate) fn has_progress(&self) -> bool {
    !self.bars.is_empty()
  }

  pub(crate) fn render_external(&self, renderer: &mut dyn ConsoleRenderer, entry: &ConsoleEntry) -> io::Result<()> {
    self.progress.suspend(|| renderer.render(entry))
  }

  pub(crate) fn tick_external(&self, renderer: &mut dyn ConsoleRenderer) -> io::Result<()> {
    self.progress.suspend(|| renderer.tick())
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for ReplacingRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    match entry.record() {
      ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { scope, .. })
        if !scope.hides_stdout() || !scope.hides_stderr() =>
      {
        self.bar(scope);
        self.render_around_progress(entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. })
        if !scope.hides_stdout() || !scope.hides_stderr() =>
      {
        let bar = self.bar(scope);
        bar.reset_elapsed();
        bar.set_message("running");
        self.render_around_progress(entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line(line),
        ..
      }) if self.raw_scope.is_none() => {
        if line.trim().is_empty() {
          return Ok(());
        }
        self.bar(scope).set_message(line.replace('\r', ""));
        Ok(())
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        payload: ConsolePayload::RawBytes(_),
        ..
      }) => {
        if self.raw_scope.as_ref() != Some(scope) {
          self.activate_raw(scope)?;
        }
        self.renderer.render(entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, status, .. }) => {
        self.finish_scope(scope, *status);
        self.render_around_progress(entry)
      },
      _ if self.raw_scope.is_some() => self.renderer.render(entry),
      _ if !self.bars.is_empty() => self.render_around_progress(entry),
      _ => self.renderer.render(entry),
    }
  }

  fn supports_raw_terminal(&self) -> bool {
    self.renderer.supports_raw_terminal()
  }

  fn tick(&mut self) -> io::Result<()> {
    if self.raw_scope.is_none() {
      for bar in self.bars.values() {
        bar.tick();
      }
    }
    self.renderer.tick()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    self.renderer.set_parallel(parallel)
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.activate_raw(scope)?;
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    if self.raw_scope.as_ref() == Some(scope) {
      self.raw_scope = None;
    }
    self.renderer.end_raw(scope)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleDiagnostic, ConsoleLevel, ConsoleRecord, ConsoleScopeAllocator, ConsoleStatus};

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

  fn renderer() -> ReplacingRenderer<Recording> {
    ReplacingRenderer::with_draw_target(Recording::default(), ProgressDrawTarget::hidden())
  }

  #[test]
  fn tracks_parallel_tasks_in_independent_rows() {
    let allocator = ConsoleScopeAllocator::default();
    let first = allocator.scope("build");
    let second = allocator.scope("test");
    let mut renderer = renderer();
    for scope in [&first, &second] {
      renderer
        .render(&ConsoleEntry::new(ConsoleRecord::Execution(
          ExecutionEvent::ScopeDeclared {
            run_id: 1,
            scope: scope.clone(),
          },
        )))
        .unwrap();
    }
    assert_eq!(renderer.bars.len(), 2);

    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 1,
        scope: Some(first.clone()),
        command_id: "build".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("compiling".to_owned()),
      })))
      .unwrap();
    assert_eq!(renderer.bars[&first].message(), "compiling");
    assert_eq!(renderer.bars[&second].message(), "waiting");
  }

  #[test]
  fn diagnostics_do_not_destroy_parallel_progress_state() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = renderer();
    renderer.bar(&scope).set_message("compiling");
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        run_id: Some(1),
        scope: Some(scope.clone()),
        level: ConsoleLevel::Warn,
        message: "warning".to_owned(),
        location: None,
      })))
      .unwrap();
    assert_eq!(renderer.bars[&scope].message(), "compiling");
    assert!(matches!(renderer.renderer.0.as_slice(), [ConsoleRecord::Diagnostic(_)]));
  }

  #[test]
  fn finishing_one_scope_keeps_other_rows_alive() {
    let allocator = ConsoleScopeAllocator::default();
    let first = allocator.scope("build");
    let second = allocator.scope("test");
    let mut renderer = renderer();
    let completed = renderer.bar(&first);
    completed.set_message("compiled");
    renderer.bar(&second);
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope: first.clone(),
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();
    assert!(!renderer.bars.contains_key(&first));
    assert!(renderer.bars.contains_key(&second));
    assert!(completed.is_finished());
    assert_eq!(completed.prefix(), "✓ [build]");
    assert_eq!(completed.message(), "compiled — done");
  }

  #[test]
  fn raw_output_clears_progress_and_passes_through_unchanged() {
    let scope = ConsoleScopeAllocator::default().scope("shell");
    let mut renderer = renderer();
    renderer.bar(&scope);
    let raw = ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope.clone()),
      command_id: "shell".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::RawBytes(b"prompt".to_vec()),
    }));
    renderer.render(&raw).unwrap();
    assert!(renderer.bars.is_empty());
    assert_eq!(renderer.raw_scope, Some(scope));
    assert_eq!(renderer.renderer.0, [raw.record().clone()]);
  }

  #[test]
  fn explicit_raw_lifecycle_suspends_and_then_restores_parallel_progress() {
    let allocator = ConsoleScopeAllocator::default();
    let interactive = allocator.scope("interactive");
    let background = allocator.scope("background");
    let mut renderer = renderer();
    renderer.bar(&background);

    ConsoleRenderer::begin_raw(&mut renderer, &interactive).unwrap();
    assert!(renderer.bars.contains_key(&background));
    assert_eq!(renderer.raw_scope, Some(interactive.clone()));

    ConsoleRenderer::end_raw(&mut renderer, &interactive).unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 1,
        scope: Some(background.clone()),
        command_id: "build".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("resumed".to_owned()),
      })))
      .unwrap();
    assert_eq!(renderer.bars[&background].message(), "resumed");
  }

  #[test]
  fn completion_labels_cover_every_status_and_clear_raw_scope() {
    let allocator = ConsoleScopeAllocator::default();
    for (status, symbol, label) in [
      (ConsoleStatus::Success, "✓", "done"),
      (ConsoleStatus::Skipped, "−", "skipped"),
      (ConsoleStatus::Cancelled, "!", "cancelled"),
      (ConsoleStatus::Failed, "✗", "failed"),
    ] {
      let scope = allocator.scope(label);
      let mut renderer = renderer();
      let bar = renderer.bar(&scope);
      renderer.raw_scope = Some(scope.clone());
      renderer.finish_scope(&scope, status);
      assert_eq!(bar.prefix(), format!("{symbol} [{label}]"));
      assert_eq!(bar.message(), label);
      assert!(renderer.raw_scope.is_none());
    }
  }

  #[test]
  fn ignores_blank_progress_lines_and_delegates_while_raw_is_active() {
    let allocator = ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let mut renderer = renderer();
    renderer.bar(&scope);
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 1,
        scope: Some(scope.clone()),
        command_id: "build".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("   ".to_owned()),
      })))
      .unwrap();
    assert_eq!(renderer.bars[&scope].message(), "waiting");

    renderer.raw_scope = Some(scope);
    let diagnostic = ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id: Some(1),
      scope: None,
      level: ConsoleLevel::Info,
      message: "raw diagnostic".to_owned(),
      location: None,
    }));
    renderer.render(&diagnostic).unwrap();
    renderer.tick().unwrap();
    assert_eq!(renderer.renderer.0, [diagnostic.record().clone()]);
  }

  #[test]
  fn delegates_capabilities_ticks_and_parallel_state() {
    let mut renderer = renderer();
    assert!(renderer.supports_raw_terminal());
    renderer.tick().unwrap();
    renderer.set_parallel(true).unwrap();
  }
}
