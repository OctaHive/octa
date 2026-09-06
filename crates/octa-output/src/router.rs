use std::{collections::HashMap, io};

use super::{
  ConsoleEntry, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStream, ExecutionEvent, GroupRenderer,
  JsonLinesRenderer, KeepOrderRenderer, OnErrorRenderer, PrefixedRenderer, RenderMode, ReplacingRenderer,
  TerminalRenderer, TimedRenderer,
};

/// Routes each task scope to its selected presentation strategy while keeping
/// all strategies behind one renderer boundary.
pub struct OutputRouterRenderer {
  default_mode: RenderMode,
  adaptive_default: bool,
  force_default: bool,
  progress_enabled: bool,
  renderers: HashMap<RenderMode, Box<dyn ConsoleRenderer>>,
  replacing: ReplacingRenderer<Box<dyn ConsoleRenderer>>,
}

pub struct OutputRouterConfig {
  pub default_mode: RenderMode,
  pub group_begin: Option<String>,
  pub group_end: Option<String>,
  pub group_error_only: bool,
  pub progress_enabled: bool,
  /// Select interleaved/prefixed after the execution plan reports its concurrency.
  pub adaptive_default: bool,
  /// CLI/environment output overrides task-local presentation when true.
  pub force_default: bool,
}

impl OutputRouterRenderer {
  pub fn new(config: OutputRouterConfig) -> Self {
    let OutputRouterConfig {
      default_mode,
      group_begin,
      group_end,
      group_error_only,
      progress_enabled,
      adaptive_default,
      force_default,
    } = config;
    let mut renderers: HashMap<RenderMode, Box<dyn ConsoleRenderer>> = HashMap::new();
    renderers.insert(RenderMode::Interleaved, Box::new(TerminalRenderer::default()));
    renderers.insert(
      RenderMode::Group,
      Box::new(GroupRenderer::with_templates(
        TerminalRenderer::default(),
        group_begin,
        group_end,
        group_error_only,
      )),
    );
    renderers.insert(
      RenderMode::Prefixed,
      Box::new(PrefixedRenderer::new(TerminalRenderer::default())),
    );
    renderers.insert(
      RenderMode::OnError,
      Box::new(OnErrorRenderer::new(TerminalRenderer::default())),
    );
    renderers.insert(
      RenderMode::KeepOrder,
      Box::new(KeepOrderRenderer::new(PrefixedRenderer::new(
        TerminalRenderer::default(),
      ))),
    );
    renderers.insert(
      RenderMode::Timed,
      Box::new(TimedRenderer::new(PrefixedRenderer::new(TerminalRenderer::default()))),
    );
    renderers.insert(RenderMode::Json, Box::new(JsonLinesRenderer::new()));

    let replacing: Box<dyn ConsoleRenderer> = Box::new(TerminalRenderer::default());
    Self {
      default_mode,
      adaptive_default,
      force_default,
      progress_enabled,
      renderers,
      replacing: ReplacingRenderer::new(replacing),
    }
  }

  fn selected_mode(&self, scope: Option<&ConsoleScope>) -> RenderMode {
    // A machine-readable stream must never be mixed with human output.
    if self.default_mode == RenderMode::Json || self.force_default {
      return self.default_mode;
    }
    scope.and_then(ConsoleScope::render_mode).unwrap_or(self.default_mode)
  }

  fn effective_mode(&self, mode: RenderMode) -> RenderMode {
    if mode == RenderMode::Replacing && !self.progress_enabled {
      RenderMode::Prefixed
    } else {
      mode
    }
  }
}

impl ConsoleRenderer for OutputRouterRenderer {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    if matches!(
      entry.record(),
      ConsoleRecord::Execution(ExecutionEvent::RunFinished { .. })
    ) && self.replacing.has_progress()
    {
      return self.replacing.render(entry);
    }
    let mode = self.effective_mode(self.selected_mode(record_scope(entry.record())));
    if mode == RenderMode::Replacing {
      return self.replacing.render(entry);
    }
    let renderer = self
      .renderers
      .get_mut(&mode)
      .expect("every non-replacing render mode has a renderer");
    if self.replacing.has_progress() {
      self.replacing.render_external(&mut **renderer, entry)
    } else {
      renderer.render(entry)
    }
  }

  fn supports_raw_terminal(&self) -> bool {
    self.default_mode != RenderMode::Json
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    if self.adaptive_default {
      self.default_mode = if parallel {
        RenderMode::Prefixed
      } else {
        RenderMode::Interleaved
      };
    }
    Ok(())
  }

  fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
    let mode = self.effective_mode(self.selected_mode(Some(scope)));
    if mode == RenderMode::Replacing {
      self.replacing.update_progress(scope, message)
    } else {
      Ok(())
    }
  }

  fn update_progress_bytes(
    &mut self,
    scope: &ConsoleScope,
    command_id: &str,
    stream: ConsoleStream,
    bytes: &[u8],
  ) -> io::Result<()> {
    let mode = self.effective_mode(self.selected_mode(Some(scope)));
    if mode == RenderMode::Replacing {
      self.replacing.update_progress_bytes(scope, command_id, stream, bytes)
    } else {
      Ok(())
    }
  }

  fn supports_progress_updates(&self) -> bool {
    self.progress_enabled
      && self.default_mode != RenderMode::Json
      && (!self.force_default || self.default_mode == RenderMode::Replacing)
  }

  fn tick(&mut self) -> io::Result<()> {
    let mut first_error = self.replacing.tick().err();
    for renderer in self.renderers.values_mut().filter(|renderer| renderer.wants_tick()) {
      let result = if self.replacing.has_progress() {
        self.replacing.tick_external(&mut **renderer)
      } else {
        renderer.tick()
      };
      if let Err(error) = result {
        first_error.get_or_insert(error);
      }
    }
    first_error.map_or(Ok(()), Err)
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    let mode = self.effective_mode(self.selected_mode(Some(scope)));
    self.replacing.begin_raw(scope)?;
    if mode == RenderMode::Replacing {
      return Ok(());
    }
    self
      .renderers
      .get_mut(&mode)
      .expect("every non-replacing render mode has a renderer")
      .begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    let mode = self.effective_mode(self.selected_mode(Some(scope)));
    let mut first_error = self.replacing.end_raw(scope).err();
    if mode != RenderMode::Replacing {
      let renderer = self
        .renderers
        .get_mut(&mode)
        .expect("every non-replacing render mode has a renderer");
      if let Err(error) = renderer.end_raw(scope) {
        first_error.get_or_insert(error);
      }
    }
    first_error.map_or(Ok(()), Err)
  }
}

fn record_scope(record: &ConsoleRecord) -> Option<&ConsoleScope> {
  match record {
    ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::StepDeclared { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::StepStarted { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::StepFinished { scope, .. })
    | ConsoleRecord::Execution(ExecutionEvent::Output { scope: Some(scope), .. })
    | ConsoleRecord::Execution(ExecutionEvent::Progress { scope: Some(scope), .. }) => Some(scope),
    ConsoleRecord::Diagnostic(diagnostic) => diagnostic.scope.as_ref(),
    _ => None,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleLevel, ConsolePayload, ConsoleScopeAllocator, ConsoleStatus, ConsoleStream};

  fn event(event: ExecutionEvent) -> ConsoleEntry {
    ConsoleEntry::new(ConsoleRecord::Execution(event))
  }

  fn router(adaptive_default: bool) -> OutputRouterRenderer {
    OutputRouterRenderer::new(OutputRouterConfig {
      default_mode: RenderMode::Interleaved,
      group_begin: None,
      group_end: None,
      group_error_only: false,
      progress_enabled: false,
      adaptive_default,
      force_default: false,
    })
  }

  #[test]
  fn adaptive_default_follows_built_plan_parallelism() {
    let mut router = router(true);
    router.set_parallel(true).unwrap();
    assert_eq!(router.default_mode, RenderMode::Prefixed);
    router.set_parallel(false).unwrap();
    assert_eq!(router.default_mode, RenderMode::Interleaved);
  }

  #[test]
  fn configured_default_ignores_plan_parallelism() {
    let mut router = router(false);
    router.set_parallel(true).unwrap();
    assert_eq!(router.default_mode, RenderMode::Interleaved);
  }

  #[test]
  fn replacing_falls_back_to_prefixed_when_progress_is_disabled() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    scope.set_render_mode(Some(RenderMode::Replacing));
    let mut router = router(false);

    assert_eq!(router.effective_mode(RenderMode::Replacing), RenderMode::Prefixed);
    router
      .render(&event(ExecutionEvent::Output {
        run_id: 1,
        scope: Some(scope),
        step_id: None,
        command_id: "build".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("compiled".to_owned()),
      }))
      .unwrap();
  }

  #[test]
  fn active_progress_suspends_other_modes_and_raw_lifecycle() {
    let allocator = ConsoleScopeAllocator::default();
    let progress_scope = allocator.scope("build");
    progress_scope.set_render_mode(Some(RenderMode::Replacing));
    let plain_scope = allocator.scope("plain");
    let mut router = OutputRouterRenderer::new(OutputRouterConfig {
      default_mode: RenderMode::Interleaved,
      group_begin: None,
      group_end: None,
      group_error_only: false,
      progress_enabled: true,
      adaptive_default: false,
      force_default: false,
    });
    assert!(router.supports_progress_updates());

    router
      .render(&event(ExecutionEvent::ScopeDeclared {
        run_id: 1,
        scope: progress_scope.clone(),
      }))
      .unwrap();
    router.update_progress(&progress_scope, "working").unwrap();
    router
      .update_progress_bytes(&progress_scope, "command", ConsoleStream::Stdout, b"partial")
      .unwrap();
    router
      .update_progress_bytes(&plain_scope, "command", ConsoleStream::Stdout, b"ignored")
      .unwrap();
    router
      .render(&event(ExecutionEvent::Output {
        run_id: 1,
        scope: Some(plain_scope.clone()),
        step_id: None,
        command_id: "plain".to_owned(),
        stream: ConsoleStream::Stderr,
        payload: ConsolePayload::Line("warning".to_owned()),
      }))
      .unwrap();
    router.tick().unwrap();
    router.begin_raw(&plain_scope).unwrap();
    router.end_raw(&plain_scope).unwrap();
    router
      .render(&event(ExecutionEvent::ScopeFinished {
        run_id: 1,
        scope: progress_scope,
        status: ConsoleStatus::Success,
      }))
      .unwrap();
  }

  #[test]
  fn json_forces_machine_output_and_disables_raw_terminal() {
    let scope = ConsoleScopeAllocator::default().scope("task");
    scope.set_render_mode(Some(RenderMode::Group));
    let router = OutputRouterRenderer::new(OutputRouterConfig {
      default_mode: RenderMode::Json,
      group_begin: None,
      group_end: None,
      group_error_only: false,
      progress_enabled: false,
      adaptive_default: false,
      force_default: false,
    });
    assert_eq!(router.selected_mode(Some(&scope)), RenderMode::Json);
    assert!(!router.supports_raw_terminal());
    assert!(!router.supports_progress_updates());

    let forced = OutputRouterRenderer::new(OutputRouterConfig {
      default_mode: RenderMode::Prefixed,
      group_begin: None,
      group_end: None,
      group_error_only: false,
      progress_enabled: false,
      adaptive_default: false,
      force_default: true,
    });
    assert_eq!(forced.selected_mode(Some(&scope)), RenderMode::Prefixed);
    assert!(forced.supports_raw_terminal());
    assert!(!forced.supports_progress_updates());
  }

  #[test]
  fn unscoped_records_use_the_default_mode() {
    let mut router = router(false);
    let diagnostic = ConsoleEntry::new(ConsoleRecord::Diagnostic(crate::ConsoleDiagnostic {
      run_id: None,
      scope: None,
      step_id: None,
      level: ConsoleLevel::Warn,
      message: "warning".to_owned(),
      location: None,
    }));
    router.render(&diagnostic).unwrap();
    assert_eq!(record_scope(diagnostic.record()), None);
  }
}
