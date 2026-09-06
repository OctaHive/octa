//! Presentation-neutral routing of one command's runtime output.
//!
//! The router attaches run/task/step identity, applies per-stream silence, and
//! forwards line, byte, diagnostic, progress, and raw-terminal events through a
//! single console boundary.

use std::{io, sync::Arc};

use octa_octafile::Silence;
use octa_output::{
  Console, ConsoleLevel, ConsolePayload, ConsoleStep, ConsoleStream, ExecutionEvent, ProgressUpdate, RawConsoleSession,
  SourceLocation,
};

#[cfg(test)]
use octa_output::ConsoleScope;

use crate::task::ExecutionBinding;

/// Associates task output and diagnostics with one execution and optional scope.
#[derive(Clone)]
pub(crate) struct RuntimeOutput {
  console: Arc<Console>,
  run_id: u64,
  binding: Option<ExecutionBinding>,
  silence: Silence,
}

impl RuntimeOutput {
  #[cfg(test)]
  pub(crate) fn new(console: Arc<Console>, run_id: u64, scope: Option<ConsoleScope>) -> Self {
    Self {
      console,
      run_id,
      binding: scope.map(ExecutionBinding::for_task),
      silence: Silence::None,
    }
  }

  pub(crate) fn with_silence(
    console: Arc<Console>,
    run_id: u64,
    binding: Option<ExecutionBinding>,
    silence: Silence,
  ) -> Self {
    Self {
      console,
      run_id,
      binding,
      silence,
    }
  }

  pub(crate) async fn line(&self, command_id: &str, stream: ConsoleStream, line: String) -> io::Result<()> {
    if self.hides(stream) {
      self
        .console
        .observe_event(self.output_event(command_id, stream, ConsolePayload::Line(line.clone())))
        .await?;
      if let Some(binding) = &self.binding {
        self.console.update_progress(binding.scope().clone(), line).await?;
      }
      return Ok(());
    }
    self
      .console
      .event(self.output_event(command_id, stream, ConsolePayload::Line(line)))
      .await
  }

  pub(crate) async fn bytes(&self, command_id: &str, stream: ConsoleStream, bytes: Vec<u8>) -> io::Result<()> {
    if self.hides(stream) {
      self
        .console
        .observe_event(self.output_event(command_id, stream, ConsolePayload::Bytes(bytes.clone())))
        .await?;
      if let Some(binding) = &self.binding {
        self
          .console
          .update_progress_bytes(binding.scope().clone(), command_id, stream, bytes)
          .await?;
      }
      return Ok(());
    }
    self
      .console
      .event(self.output_event(command_id, stream, ConsolePayload::Bytes(bytes)))
      .await
  }

  fn output_event(&self, command_id: &str, stream: ConsoleStream, payload: ConsolePayload) -> ExecutionEvent {
    ExecutionEvent::Output {
      run_id: self.run_id,
      scope: self.binding.as_ref().map(|binding| binding.scope().clone()),
      step_id: self
        .binding
        .as_ref()
        .and_then(ExecutionBinding::step)
        .map(ConsoleStep::id),
      command_id: command_id.to_owned(),
      stream,
      payload,
    }
  }

  pub(crate) async fn progress(&self, command_id: &str, progress: ProgressUpdate) -> io::Result<()> {
    self
      .console
      .event(ExecutionEvent::Progress {
        run_id: self.run_id,
        scope: self.binding.as_ref().map(|binding| binding.scope().clone()),
        step_id: self
          .binding
          .as_ref()
          .and_then(ExecutionBinding::step)
          .map(ConsoleStep::id),
        command_id: command_id.to_owned(),
        progress,
      })
      .await
  }

  pub(crate) async fn begin_raw(&self, command_id: &str) -> io::Result<Option<RawConsoleSession>> {
    let Some(binding) = &self.binding else {
      return Ok(None);
    };
    self
      .console
      .begin_raw_for_step(
        self.run_id,
        binding.scope().clone(),
        binding.step().map(ConsoleStep::id),
        command_id,
      )
      .await
      .map(Some)
  }

  pub(crate) fn hides(&self, stream: ConsoleStream) -> bool {
    match stream {
      ConsoleStream::Stdout => self.silence.hides_stdout(),
      ConsoleStream::Stderr => self.silence.hides_stderr(),
    }
  }

  pub(crate) async fn message(&self, level: ConsoleLevel, message: impl Into<String>) -> io::Result<()> {
    self.diagnostic(level, message, None).await
  }

  pub(crate) async fn diagnostic(
    &self,
    level: ConsoleLevel,
    message: impl Into<String>,
    location: Option<SourceLocation>,
  ) -> io::Result<()> {
    match &self.binding {
      Some(binding) if let Some(step) = binding.step() => {
        self
          .console
          .run_diagnostic_at_step(
            self.run_id,
            binding.scope().clone(),
            step.id(),
            level,
            message,
            location,
          )
          .await
      },
      Some(binding) => {
        self
          .console
          .run_diagnostic_at(self.run_id, binding.scope().clone(), level, message, location)
          .await
      },
      None => self.console.run_diagnostic(self.run_id, level, message, location).await,
    }
  }
}

#[cfg(test)]
mod tests {
  use std::sync::Mutex;

  use octa_output::{ConsoleEntry, ConsoleRecord, ConsoleRenderer, ConsoleScopeAllocator};

  use super::*;

  type RecordedProgressBytes = (ConsoleScope, String, ConsoleStream, Vec<u8>);

  #[derive(Clone, Default)]
  struct Recording {
    records: Arc<Mutex<Vec<ConsoleRecord>>>,
    progress: Arc<Mutex<Vec<(ConsoleScope, String)>>>,
    progress_bytes: Arc<Mutex<Vec<RecordedProgressBytes>>>,
  }

  impl ConsoleRenderer for Recording {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.records.lock().unwrap().push(entry.record().clone());
      Ok(())
    }

    fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
      self.progress.lock().unwrap().push((scope.clone(), message.to_owned()));
      Ok(())
    }

    fn update_progress_bytes(
      &mut self,
      scope: &ConsoleScope,
      command_id: &str,
      stream: ConsoleStream,
      bytes: &[u8],
    ) -> io::Result<()> {
      self
        .progress_bytes
        .lock()
        .unwrap()
        .push((scope.clone(), command_id.to_owned(), stream, bytes.to_vec()));
      Ok(())
    }

    fn supports_progress_updates(&self) -> bool {
      true
    }
  }

  #[tokio::test]
  async fn routes_bytes_and_honors_per_stream_silence() {
    let records = Recording::default();
    let console = Arc::new(Console::new(records.clone()));
    let scope = ConsoleScopeAllocator::default().scope("task");
    let stdout_hidden = RuntimeOutput::with_silence(
      console.clone(),
      7,
      Some(ExecutionBinding::for_task(scope.clone())),
      Silence::Stdout,
    );
    let stderr_hidden = RuntimeOutput::with_silence(
      console.clone(),
      7,
      Some(ExecutionBinding::for_task(scope.clone())),
      Silence::Stderr,
    );

    stdout_hidden
      .bytes("command", ConsoleStream::Stdout, b"hidden".to_vec())
      .await
      .unwrap();
    stdout_hidden
      .bytes("command", ConsoleStream::Stderr, b"stderr".to_vec())
      .await
      .unwrap();
    stderr_hidden
      .bytes("command", ConsoleStream::Stdout, b"stdout".to_vec())
      .await
      .unwrap();
    stderr_hidden
      .bytes("command", ConsoleStream::Stderr, b"hidden".to_vec())
      .await
      .unwrap();
    console.drain().await.unwrap();

    assert!(stdout_hidden.hides(ConsoleStream::Stdout));
    assert!(!stdout_hidden.hides(ConsoleStream::Stderr));
    assert!(!stderr_hidden.hides(ConsoleStream::Stdout));
    assert!(stderr_hidden.hides(ConsoleStream::Stderr));
    assert_eq!(records.records.lock().unwrap().len(), 2);
    assert_eq!(
      *records.progress_bytes.lock().unwrap(),
      [
        (
          scope.clone(),
          "command".to_owned(),
          ConsoleStream::Stdout,
          b"hidden".to_vec()
        ),
        (scope, "command".to_owned(), ConsoleStream::Stderr, b"hidden".to_vec())
      ]
    );
  }

  #[tokio::test]
  async fn hidden_streams_update_progress_without_emitting_output_records() {
    let records = Recording::default();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let sink_records = observed.clone();
    let console = Arc::new(Console::with_event_sink(
      records.clone(),
      move |entry: &ConsoleEntry| {
        sink_records.lock().unwrap().push(entry.record().clone());
        Ok(())
      },
    ));
    let scope = ConsoleScopeAllocator::default().scope("task");
    let target = RuntimeOutput::with_silence(
      console,
      7,
      Some(ExecutionBinding::for_task(scope.clone())),
      Silence::All,
    );

    target
      .line("command", ConsoleStream::Stdout, "compiling".to_owned())
      .await
      .unwrap();
    target
      .line("command", ConsoleStream::Stderr, "linking".to_owned())
      .await
      .unwrap();

    assert_eq!(
      *records.progress.lock().unwrap(),
      [(scope.clone(), "compiling".to_owned()), (scope, "linking".to_owned())]
    );
    assert!(records.records.lock().unwrap().is_empty());
    assert_eq!(observed.lock().unwrap().len(), 2);
  }

  #[tokio::test]
  async fn structured_progress_is_emitted_for_a_silent_step() {
    let records = Recording::default();
    let console = Arc::new(Console::new(records.clone()));
    let allocator = ConsoleScopeAllocator::default();
    let scope = allocator.scope("task");
    let step = allocator.step(&scope, "compile");
    let target = RuntimeOutput::with_silence(
      console.clone(),
      7,
      Some(ExecutionBinding::for_step(scope.clone(), step.clone())),
      Silence::All,
    );

    target
      .progress(
        "command",
        ProgressUpdate {
          message: "Compiling".to_owned(),
          current: Some(1),
          total: Some(2),
          unit: Some("files".to_owned()),
        },
      )
      .await
      .unwrap();
    console.drain().await.unwrap();

    assert!(matches!(
      records.records.lock().unwrap().as_slice(),
      [ConsoleRecord::Execution(ExecutionEvent::Progress {
        run_id: 7,
        scope: Some(actual_scope),
        step_id: Some(actual_step),
        ..
      })] if actual_scope == &scope && *actual_step == step.id()
    ));
  }

  #[tokio::test]
  async fn starts_raw_only_for_scoped_targets() {
    let console = Arc::new(Console::default());
    let unscoped = RuntimeOutput::new(console.clone(), 1, None);
    assert!(unscoped.begin_raw("command").await.unwrap().is_none());

    let scope = ConsoleScopeAllocator::default().scope("raw");
    let scoped = RuntimeOutput::new(console, 1, Some(scope));
    assert!(scoped.begin_raw("command").await.unwrap().is_some());
  }
}
