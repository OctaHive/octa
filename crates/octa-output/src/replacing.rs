use std::{borrow::Cow, collections::hash_map::Entry, collections::HashMap, io, time::Instant};

use console::{measure_text_width, truncate_str, Term};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use super::{
  ConsoleEntry, ConsoleLevel, ConsolePayload, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStatus,
  ConsoleStream, ExecutionEvent,
};

const SPINNER_TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const PROGRESS_LINE_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy)]
enum RowPhase {
  Waiting,
  Running,
  Finished(ConsoleStatus),
}

struct ProgressRow {
  phase: RowPhase,
  message: String,
  started: Instant,
  streamed_lines: HashMap<ConsoleStream, HashMap<String, StreamedLine>>,
}

impl ProgressRow {
  fn waiting() -> Self {
    Self {
      phase: RowPhase::Waiting,
      message: "waiting".to_owned(),
      started: Instant::now(),
      streamed_lines: HashMap::new(),
    }
  }

  fn complete_line(&mut self, command_id: &str, stream: ConsoleStream, line: &str) -> Option<String> {
    if let Some(lines) = self.streamed_lines.get_mut(&stream) {
      lines.remove(command_id);
    }
    normalized_message(line)
  }

  fn push_bytes(&mut self, command_id: &str, stream: ConsoleStream, bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
      return None;
    }
    let lines = self.streamed_lines.entry(stream).or_default();
    if !lines.contains_key(command_id) {
      lines.insert(command_id.to_owned(), StreamedLine::default());
    }
    let message = lines
      .get_mut(command_id)
      .expect("streamed line was inserted above")
      .push(bytes);
    if bytes.last() == Some(&b'\n') {
      lines.remove(command_id);
    }
    message
  }
}

#[derive(Default)]
struct StreamedLine {
  bytes: Vec<u8>,
}

impl StreamedLine {
  fn push(&mut self, bytes: &[u8]) -> Option<String> {
    let mut message = None;
    for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
      let complete = segment.last() == Some(&b'\n');
      let content = if complete {
        &segment[..segment.len().saturating_sub(1)]
      } else {
        segment
      };
      self.extend_bounded(content);
      if let Some(current) = display_message(&self.bytes) {
        message = Some(current);
      }
      if complete {
        self.bytes.clear();
      }
    }
    message
  }

  fn extend_bounded(&mut self, bytes: &[u8]) {
    if bytes.len() >= PROGRESS_LINE_LIMIT {
      self.bytes.clear();
      self
        .bytes
        .extend_from_slice(&bytes[bytes.len() - PROGRESS_LINE_LIMIT..]);
      return;
    }
    let excess = self
      .bytes
      .len()
      .saturating_add(bytes.len())
      .saturating_sub(PROGRESS_LINE_LIMIT);
    if excess != 0 {
      self.bytes.drain(..excess);
    }
    self.bytes.extend_from_slice(bytes);
  }
}

fn display_message(bytes: &[u8]) -> Option<String> {
  let text = match std::str::from_utf8(bytes) {
    Ok(text) => Cow::Borrowed(text),
    Err(error) if error.error_len().is_none() => {
      Cow::Borrowed(std::str::from_utf8(&bytes[..error.valid_up_to()]).expect("valid UTF-8 prefix was reported"))
    },
    Err(_) => String::from_utf8_lossy(bytes),
  };
  normalized_message(&text)
}

fn normalized_message(message: &str) -> Option<String> {
  (!message.trim().is_empty()).then(|| message.replace('\r', ""))
}

/// Atomically redraws one multi-line progress frame containing one row per task.
pub struct ReplacingRenderer<R> {
  renderer: R,
  progress: ProgressBar,
  draw_target: Box<dyn Fn() -> ProgressDrawTarget + Send>,
  terminal_width: Box<dyn Fn() -> usize + Send>,
  rows: HashMap<ConsoleScope, ProgressRow>,
  order: Vec<ConsoleScope>,
  tick: usize,
  raw_scope: Option<ConsoleScope>,
}

impl<R> ReplacingRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self::with_draw_target(renderer, ProgressDrawTarget::stderr, || {
      Term::stderr().size().1 as usize
    })
  }

  fn with_draw_target(
    renderer: R,
    draw_target: impl Fn() -> ProgressDrawTarget + Send + 'static,
    terminal_width: impl Fn() -> usize + Send + 'static,
  ) -> Self {
    let progress = Self::progress_bar(draw_target());
    Self {
      renderer,
      progress,
      draw_target: Box::new(draw_target),
      terminal_width: Box::new(terminal_width),
      rows: HashMap::new(),
      order: Vec::new(),
      tick: 0,
      raw_scope: None,
    }
  }

  fn progress_bar(target: ProgressDrawTarget) -> ProgressBar {
    ProgressBar::with_draw_target(None, target)
      .with_style(ProgressStyle::with_template("{msg}").expect("static replacing progress template is valid"))
  }

  fn row(&mut self, scope: &ConsoleScope) -> &mut ProgressRow {
    match self.rows.entry(scope.clone()) {
      Entry::Occupied(entry) => entry.into_mut(),
      Entry::Vacant(entry) => {
        self.order.push(scope.clone());
        entry.insert(ProgressRow::waiting())
      },
    }
  }

  fn draw(&self) {
    if self.rows.is_empty() || self.raw_scope.is_some() {
      return;
    }
    let width = (self.terminal_width)();
    let mut frame = String::new();
    for scope in &self.order {
      let Some(row) = self.rows.get(scope) else {
        continue;
      };
      if !frame.is_empty() {
        frame.push('\n');
      }
      frame.push_str(&self.format_row(scope, row, width));
    }
    self.progress.set_message(frame);
  }

  fn format_row(&self, scope: &ConsoleScope, row: &ProgressRow, width: usize) -> String {
    let (symbol, message) = match row.phase {
      RowPhase::Waiting | RowPhase::Running => (
        SPINNER_TICKS[self.tick % SPINNER_TICKS.len()],
        Cow::Borrowed(row.message.as_str()),
      ),
      RowPhase::Finished(status) => {
        let (symbol, label) = status_label(status);
        let message = if matches!(row.message.as_str(), "waiting" | "running") {
          Cow::Borrowed(label)
        } else {
          Cow::Owned(format!("{} — {label}", row.message))
        };
        (symbol, message)
      },
    };
    let elapsed = row.started.elapsed().as_secs();
    let hours = elapsed / 3_600;
    let minutes = elapsed % 3_600 / 60;
    let seconds = elapsed % 60;
    let body = format!("{symbol} [{}] {message}", scope.prefix());
    let elapsed = format!("[{hours:02}:{minutes:02}:{seconds:02}]");
    let elapsed_width = measure_text_width(&elapsed);
    if width <= elapsed_width + 1 {
      return truncate_str(&format!("{body} {elapsed}"), width, "…").into_owned();
    }
    let body_width = width - elapsed_width - 1;
    let body = truncate_str(&body, body_width, "…");
    let padding = body_width.saturating_sub(measure_text_width(&body));
    format!("{body}{}{elapsed}", " ".repeat(padding + 1))
  }

  fn finish_scope(&mut self, scope: &ConsoleScope, status: ConsoleStatus) {
    if let Some(row) = self.rows.get_mut(scope) {
      row.phase = RowPhase::Finished(status);
      row.streamed_lines.clear();
    }
    if self.raw_scope.as_ref() == Some(scope) {
      self.raw_scope = None;
    }
    self.draw();
  }

  fn finish_rows(&mut self) {
    self.progress.finish();
    self.rows.clear();
    self.order.clear();
    self.tick = 0;
    self.raw_scope = None;
    self.progress = Self::progress_bar((self.draw_target)());
  }

  fn activate_raw(&mut self, scope: &ConsoleScope) {
    self.progress.finish_and_clear();
    self.rows.remove(scope);
    self.order.retain(|candidate| candidate != scope);
    self.progress = Self::progress_bar((self.draw_target)());
    self.raw_scope = Some(scope.clone());
  }

  fn render_around_progress(&mut self, entry: &ConsoleEntry) -> io::Result<()>
  where
    R: ConsoleRenderer,
  {
    let renderer = &mut self.renderer;
    self.progress.suspend(|| renderer.render(entry))
  }

  pub(crate) fn has_progress(&self) -> bool {
    !self.rows.is_empty()
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
      ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { scope, .. }) => {
        self.row(scope);
        self.draw();
        Ok(())
      },
      ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. }) => {
        let row = self.row(scope);
        row.phase = RowPhase::Running;
        row.message = "running".to_owned();
        row.started = Instant::now();
        self.draw();
        Ok(())
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        command_id,
        stream,
        payload: ConsolePayload::Line(line),
        ..
      }) if self.raw_scope.is_none() => {
        if let Some(message) = self.row(scope).complete_line(command_id, *stream, line) {
          self.row(scope).message = message;
          self.draw();
        }
        self.render_around_progress(entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        command_id,
        stream,
        payload: ConsolePayload::Bytes(bytes),
        ..
      }) if self.raw_scope.is_none() => {
        if let Some(message) = self.row(scope).push_bytes(command_id, *stream, bytes) {
          self.row(scope).message = message;
          self.draw();
        }
        self.render_around_progress(entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        payload: ConsolePayload::RawBytes(_),
        ..
      }) => {
        if self.raw_scope.as_ref() != Some(scope) {
          self.activate_raw(scope);
        }
        self.renderer.render(entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, status, .. }) => {
        self.finish_scope(scope, *status);
        Ok(())
      },
      ConsoleRecord::Execution(ExecutionEvent::RunFinished { .. }) => {
        self.finish_rows();
        self.renderer.render(entry)
      },
      ConsoleRecord::Diagnostic(diagnostic)
        if matches!(
          diagnostic.level,
          ConsoleLevel::Trace | ConsoleLevel::Debug | ConsoleLevel::Info
        ) && diagnostic
          .scope
          .as_ref()
          .is_some_and(|scope| self.rows.contains_key(scope)) =>
      {
        Ok(())
      },
      _ if self.raw_scope.is_some() => self.renderer.render(entry),
      _ if !self.rows.is_empty() => self.render_around_progress(entry),
      _ => self.renderer.render(entry),
    }
  }

  fn supports_raw_terminal(&self) -> bool {
    self.renderer.supports_raw_terminal()
  }

  fn tick(&mut self) -> io::Result<()> {
    if self.raw_scope.is_none() {
      self.tick = self.tick.wrapping_add(1);
      self.draw();
    }
    self.renderer.tick()
  }

  fn wants_tick(&self) -> bool {
    !self.rows.is_empty() || self.renderer.wants_tick()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    self.renderer.set_parallel(parallel)
  }

  fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
    if self.raw_scope.is_none() {
      if let Some(row) = self.rows.get_mut(scope) {
        row.streamed_lines.clear();
        if let Some(message) = normalized_message(message) {
          row.message = message;
          self.draw();
        }
      }
    }
    Ok(())
  }

  fn update_progress_bytes(
    &mut self,
    scope: &ConsoleScope,
    command_id: &str,
    stream: ConsoleStream,
    bytes: &[u8],
  ) -> io::Result<()> {
    if self.raw_scope.is_none() {
      if let Some(row) = self.rows.get_mut(scope) {
        if let Some(message) = row.push_bytes(command_id, stream, bytes) {
          row.message = message;
          self.draw();
        }
      }
    }
    Ok(())
  }

  fn supports_progress_updates(&self) -> bool {
    true
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.activate_raw(scope);
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    if self.raw_scope.as_ref() == Some(scope) {
      self.raw_scope = None;
      self.draw();
    }
    self.renderer.end_raw(scope)
  }
}

fn status_label(status: ConsoleStatus) -> (&'static str, &'static str) {
  match status {
    ConsoleStatus::Success => ("✓", "done"),
    ConsoleStatus::Skipped => ("−", "skipped"),
    ConsoleStatus::Cancelled => ("!", "cancelled"),
    ConsoleStatus::Failed => ("✗", "failed"),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleDiagnostic, ConsoleLevel, ConsoleRecord, ConsoleScopeAllocator, ConsoleStatus, ConsoleStream};
  use indicatif::InMemoryTerm;

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

  struct InspectingRenderer {
    terminal: InMemoryTerm,
    contents_when_rendered: Option<String>,
  }

  impl ConsoleRenderer for InspectingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      if matches!(
        entry.record(),
        ConsoleRecord::Execution(ExecutionEvent::RunFinished { .. })
      ) {
        self.contents_when_rendered = Some(self.terminal.contents());
      }
      Ok(())
    }
  }

  fn renderer() -> ReplacingRenderer<Recording> {
    ReplacingRenderer::with_draw_target(Recording::default(), ProgressDrawTarget::hidden, || 80)
  }

  #[test]
  fn visible_output_updates_progress_and_remains_visible() {
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
    assert_eq!(renderer.rows.len(), 2);

    let output = ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(first.clone()),
      step_id: None,
      command_id: "build".to_owned(),
      stream: ConsoleStream::Stderr,
      payload: ConsolePayload::Line("compiling".to_owned()),
    });
    renderer.render(&ConsoleEntry::new(output.clone())).unwrap();
    assert_eq!(renderer.rows[&first].message, "compiling");
    assert_eq!(renderer.rows[&second].message, "waiting");
    assert_eq!(renderer.renderer.0, [output]);
  }

  #[test]
  fn streamed_bytes_update_the_current_line_without_corrupting_split_utf8() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = renderer();
    renderer.row(&scope).phase = RowPhase::Running;
    let text = "сборка".as_bytes();

    for bytes in [&text[..1], &text[1..4], &text[4..]] {
      renderer
        .render(&ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
          run_id: 1,
          scope: Some(scope.clone()),
          step_id: None,
          command_id: "build".to_owned(),
          stream: ConsoleStream::Stdout,
          payload: ConsolePayload::Bytes(bytes.to_vec()),
        })))
        .unwrap();
    }

    assert_eq!(renderer.rows[&scope].message, "сборка");
    assert_eq!(renderer.renderer.0.len(), 3);
  }

  #[test]
  fn keeps_partial_stdout_and_stderr_lines_independent() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = renderer();
    renderer.row(&scope).phase = RowPhase::Running;

    renderer
      .update_progress_bytes(&scope, "command", ConsoleStream::Stdout, b"out")
      .unwrap();
    renderer
      .update_progress_bytes(&scope, "command", ConsoleStream::Stderr, b"warning")
      .unwrap();
    renderer
      .update_progress_bytes(&scope, "command", ConsoleStream::Stdout, b"put")
      .unwrap();

    assert_eq!(renderer.rows[&scope].message, "output");
  }

  #[test]
  fn tracks_silent_tasks_without_exposing_their_output() {
    let scope = ConsoleScopeAllocator::default().scope_with_options("build", None, true, true);
    let mut renderer = renderer();

    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeDeclared {
          run_id: 1,
          scope: scope.clone(),
        },
      )))
      .unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeStarted {
          run_id: 1,
          scope: scope.clone(),
        },
      )))
      .unwrap();

    assert_eq!(renderer.rows[&scope].message, "running");
    renderer.update_progress(&scope, "compiling\r").unwrap();
    assert_eq!(renderer.rows[&scope].message, "compiling");
    renderer
      .update_progress_bytes(&scope, "build", ConsoleStream::Stdout, b"link")
      .unwrap();
    renderer
      .update_progress_bytes(&scope, "build", ConsoleStream::Stdout, b"ing\npack")
      .unwrap();
    assert_eq!(renderer.rows[&scope].message, "pack");
    assert!(renderer
      .renderer
      .0
      .iter()
      .all(|record| !matches!(record, ConsoleRecord::Execution(ExecutionEvent::Output { .. }))));
  }

  #[test]
  fn streamed_progress_retains_only_a_bounded_line_tail() {
    let mut line = StreamedLine::default();
    line.push(&vec![b'a'; PROGRESS_LINE_LIMIT + 10]);

    assert_eq!(line.bytes.len(), PROGRESS_LINE_LIMIT);
  }

  #[test]
  fn suppresses_scoped_info_but_preserves_warnings_during_progress() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = renderer();
    renderer.row(&scope);

    for level in [ConsoleLevel::Info, ConsoleLevel::Warn] {
      renderer
        .render(&ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
          run_id: Some(1),
          scope: Some(scope.clone()),
          step_id: None,
          level,
          message: "message".to_owned(),
          location: None,
        })))
        .unwrap();
    }

    assert!(matches!(
      renderer.renderer.0.as_slice(),
      [ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        level: ConsoleLevel::Warn,
        ..
      })]
    ));
  }

  #[test]
  fn diagnostics_do_not_destroy_parallel_progress_state() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = renderer();
    renderer.row(&scope).message = "compiling".to_owned();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        run_id: Some(1),
        scope: Some(scope.clone()),
        step_id: None,
        level: ConsoleLevel::Warn,
        message: "warning".to_owned(),
        location: None,
      })))
      .unwrap();
    assert_eq!(renderer.rows[&scope].message, "compiling");
    assert!(matches!(renderer.renderer.0.as_slice(), [ConsoleRecord::Diagnostic(_)]));
  }

  #[test]
  fn finishing_one_scope_keeps_other_rows_alive() {
    let allocator = ConsoleScopeAllocator::default();
    let first = allocator.scope("build");
    let second = allocator.scope("test");
    let mut renderer = renderer();
    renderer.row(&first).message = "compiled".to_owned();
    renderer.row(&second);
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope: first.clone(),
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();
    assert!(renderer.rows.contains_key(&first));
    assert!(renderer.rows.contains_key(&second));
    assert!(matches!(
      renderer.rows[&first].phase,
      RowPhase::Finished(ConsoleStatus::Success)
    ));
    assert!(matches!(renderer.rows[&second].phase, RowPhase::Waiting));
    assert!(renderer
      .format_row(&first, &renderer.rows[&first], 80)
      .starts_with("✓ [build] compiled — done"));
  }

  #[test]
  fn out_of_order_completion_keeps_every_row_until_the_run_is_complete() {
    let allocator = ConsoleScopeAllocator::default();
    let scopes = [
      allocator.scope("parent"),
      allocator.scope("first"),
      allocator.scope("second"),
    ];
    let mut renderer = renderer();
    for scope in &scopes {
      renderer.row(scope);
    }

    for scope in scopes.iter().rev() {
      renderer.finish_scope(scope, ConsoleStatus::Success);
      assert_eq!(renderer.rows.len(), 3);
    }

    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::RunFinished {
          run_id: 1,
          command: "parent".to_owned(),
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();
    assert!(renderer.rows.is_empty());
    assert!(renderer.order.is_empty());
    assert!(matches!(
      renderer.renderer.0.as_slice(),
      [ConsoleRecord::Execution(ExecutionEvent::RunFinished { .. })]
    ));
  }

  #[test]
  fn out_of_order_completion_leaves_one_final_row_per_declared_scope() {
    let terminal = InMemoryTerm::new(10, 80);
    let draw_terminal = terminal.clone();
    let mut renderer = ReplacingRenderer::with_draw_target(
      Recording::default(),
      move || ProgressDrawTarget::term_like(Box::new(draw_terminal.clone())),
      || 80,
    );
    let allocator = ConsoleScopeAllocator::default();
    let scopes = [
      allocator.scope("parent"),
      allocator.scope("first"),
      allocator.scope("second"),
    ];

    for scope in &scopes {
      renderer
        .render(&ConsoleEntry::new(ConsoleRecord::Execution(
          ExecutionEvent::ScopeDeclared {
            run_id: 1,
            scope: scope.clone(),
          },
        )))
        .unwrap();
    }
    assert_eq!(
      terminal
        .contents()
        .lines()
        .filter(|line| line.contains("waiting"))
        .count(),
      3
    );

    for scope in &scopes {
      renderer
        .render(&ConsoleEntry::new(ConsoleRecord::Execution(
          ExecutionEvent::ScopeStarted {
            run_id: 1,
            scope: scope.clone(),
          },
        )))
        .unwrap();
    }
    assert_eq!(
      terminal
        .contents()
        .lines()
        .filter(|line| line.contains("running"))
        .count(),
      3
    );
    assert!(!terminal.contents().contains("waiting"));
    renderer.update_progress(&scopes[1], "first output").unwrap();
    renderer.update_progress(&scopes[2], "second output").unwrap();
    for scope in scopes.iter().rev() {
      renderer.finish_scope(scope, ConsoleStatus::Success);
    }
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::RunFinished {
          run_id: 1,
          command: "parent".to_owned(),
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();

    assert_eq!(
      terminal.contents(),
      "✓ [parent] done                                                       [00:00:00]\n\
       ✓ [first] first output — done                                         [00:00:00]\n\
       ✓ [second] second output — done                                       [00:00:00]"
    );
  }

  #[test]
  fn commits_final_rows_before_rendering_the_run_summary() {
    let terminal = InMemoryTerm::new(10, 80);
    let draw_terminal = terminal.clone();
    let inspecting_terminal = terminal.clone();
    let mut renderer = ReplacingRenderer::with_draw_target(
      InspectingRenderer {
        terminal: inspecting_terminal,
        contents_when_rendered: None,
      },
      move || ProgressDrawTarget::term_like(Box::new(draw_terminal.clone())),
      || 80,
    );
    let scope = ConsoleScopeAllocator::default().scope("build");

    renderer.row(&scope);
    renderer.finish_scope(&scope, ConsoleStatus::Success);
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::RunFinished {
          run_id: 1,
          command: "build".to_owned(),
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();

    assert_eq!(
      renderer.renderer.contents_when_rendered.as_deref(),
      Some("✓ [build] done                                                        [00:00:00]")
    );
  }

  #[test]
  fn rows_are_truncated_to_one_terminal_line() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = renderer();
    renderer.row(&scope).message = "a very long compiler progress message".to_owned();

    let row = renderer.format_row(&scope, &renderer.rows[&scope], 30);

    assert_eq!(measure_text_width(&row), 30);
    assert!(row.contains('…'));
    assert!(row.ends_with("[00:00:00]"));
  }

  #[test]
  fn raw_output_clears_progress_and_passes_through_unchanged() {
    let scope = ConsoleScopeAllocator::default().scope("shell");
    let mut renderer = renderer();
    renderer.row(&scope);
    let raw = ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope.clone()),
      step_id: None,
      command_id: "shell".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::RawBytes(b"prompt".to_vec()),
    }));
    renderer.render(&raw).unwrap();
    assert!(renderer.rows.is_empty());
    assert_eq!(renderer.raw_scope, Some(scope));
    assert_eq!(renderer.renderer.0, [raw.record().clone()]);
  }

  #[test]
  fn explicit_raw_lifecycle_suspends_and_then_restores_parallel_progress() {
    let allocator = ConsoleScopeAllocator::default();
    let interactive = allocator.scope("interactive");
    let background = allocator.scope("background");
    let mut renderer = renderer();
    renderer.row(&background);

    ConsoleRenderer::begin_raw(&mut renderer, &interactive).unwrap();
    assert!(renderer.rows.contains_key(&background));
    assert_eq!(renderer.raw_scope, Some(interactive.clone()));

    ConsoleRenderer::end_raw(&mut renderer, &interactive).unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 1,
        scope: Some(background.clone()),
        step_id: None,
        command_id: "build".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("resumed".to_owned()),
      })))
      .unwrap();
    assert_eq!(renderer.rows[&background].message, "resumed");
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
      renderer.row(&scope);
      renderer.raw_scope = Some(scope.clone());
      renderer.finish_scope(&scope, status);
      assert!(renderer
        .format_row(&scope, &renderer.rows[&scope], 80)
        .starts_with(&format!("{symbol} [{label}] {label}")));
      assert!(renderer.raw_scope.is_none());
    }
  }

  #[test]
  fn blank_output_does_not_replace_status_but_remains_visible() {
    let allocator = ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let mut renderer = renderer();
    renderer.row(&scope);
    let blank = ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope.clone()),
      step_id: None,
      command_id: "build".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Line("   ".to_owned()),
    });
    renderer.render(&ConsoleEntry::new(blank.clone())).unwrap();
    assert_eq!(renderer.rows[&scope].message, "waiting");
    assert_eq!(renderer.renderer.0.as_slice(), std::slice::from_ref(&blank));

    renderer.raw_scope = Some(scope);
    let diagnostic = ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id: Some(1),
      scope: None,
      step_id: None,
      level: ConsoleLevel::Info,
      message: "raw diagnostic".to_owned(),
      location: None,
    }));
    renderer.render(&diagnostic).unwrap();
    renderer.tick().unwrap();
    assert_eq!(renderer.renderer.0, [blank, diagnostic.record().clone()]);
  }

  #[test]
  fn delegates_capabilities_ticks_and_parallel_state() {
    let mut renderer = renderer();
    assert!(renderer.supports_raw_terminal());
    renderer.tick().unwrap();
    renderer.set_parallel(true).unwrap();
  }
}
