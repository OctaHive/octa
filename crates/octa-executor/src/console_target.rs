use std::{io, sync::Arc};

use octa_octafile::Silence;
use octa_output::{
  Console, ConsoleLevel, ConsolePayload, ConsoleScope, ConsoleStream, ExecutionEvent, RawConsoleSession, SourceLocation,
};

/// Associates task output and diagnostics with one execution and optional scope.
#[derive(Clone)]
pub(crate) struct ConsoleTarget {
  console: Arc<Console>,
  run_id: u64,
  scope: Option<ConsoleScope>,
  silence: Silence,
}

impl ConsoleTarget {
  #[cfg(test)]
  pub(crate) fn new(console: Arc<Console>, run_id: u64, scope: Option<ConsoleScope>) -> Self {
    Self {
      console,
      run_id,
      scope,
      silence: Silence::None,
    }
  }

  pub(crate) fn with_silence(
    console: Arc<Console>,
    run_id: u64,
    scope: Option<ConsoleScope>,
    silence: Silence,
  ) -> Self {
    Self {
      console,
      run_id,
      scope,
      silence,
    }
  }

  pub(crate) async fn line(&self, command_id: &str, stream: ConsoleStream, line: String) -> io::Result<()> {
    if matches!(stream, ConsoleStream::Stdout) && self.silence.hides_stdout()
      || matches!(stream, ConsoleStream::Stderr) && self.silence.hides_stderr()
    {
      if let Some(scope) = &self.scope {
        self.console.update_progress(scope.clone(), line).await?;
      }
      return Ok(());
    }
    self
      .console
      .event(ExecutionEvent::Output {
        run_id: self.run_id,
        scope: self.scope.clone(),
        command_id: command_id.to_owned(),
        stream,
        payload: ConsolePayload::Line(line),
      })
      .await
  }

  pub(crate) async fn bytes(&self, command_id: &str, stream: ConsoleStream, bytes: Vec<u8>) -> io::Result<()> {
    if matches!(stream, ConsoleStream::Stdout) && self.silence.hides_stdout()
      || matches!(stream, ConsoleStream::Stderr) && self.silence.hides_stderr()
    {
      return Ok(());
    }
    self
      .console
      .event(ExecutionEvent::Output {
        run_id: self.run_id,
        scope: self.scope.clone(),
        command_id: command_id.to_owned(),
        stream,
        payload: ConsolePayload::Bytes(bytes),
      })
      .await
  }

  pub(crate) async fn begin_raw(&self, command_id: &str) -> io::Result<Option<RawConsoleSession>> {
    let Some(scope) = &self.scope else {
      return Ok(None);
    };
    self
      .console
      .begin_raw(self.run_id, scope.clone(), command_id)
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
    match &self.scope {
      Some(scope) => {
        self
          .console
          .run_diagnostic_at(self.run_id, scope.clone(), level, message, location)
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

  #[derive(Clone, Default)]
  struct Recording {
    records: Arc<Mutex<Vec<ConsoleRecord>>>,
    progress: Arc<Mutex<Vec<(ConsoleScope, String)>>>,
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

    fn supports_progress_updates(&self) -> bool {
      true
    }
  }

  #[tokio::test]
  async fn routes_bytes_and_honors_per_stream_silence() {
    let records = Recording::default();
    let console = Arc::new(Console::new(records.clone()));
    let scope = ConsoleScopeAllocator::default().scope("task");
    let stdout_hidden = ConsoleTarget::with_silence(console.clone(), 7, Some(scope.clone()), Silence::Stdout);
    let stderr_hidden = ConsoleTarget::with_silence(console.clone(), 7, Some(scope.clone()), Silence::Stderr);

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
  }

  #[tokio::test]
  async fn hidden_streams_update_progress_without_emitting_output_records() {
    let records = Recording::default();
    let console = Arc::new(Console::new(records.clone()));
    let scope = ConsoleScopeAllocator::default().scope("task");
    let target = ConsoleTarget::with_silence(console, 7, Some(scope.clone()), Silence::All);

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
  }

  #[tokio::test]
  async fn starts_raw_only_for_scoped_targets() {
    let console = Arc::new(Console::default());
    let unscoped = ConsoleTarget::new(console.clone(), 1, None);
    assert!(unscoped.begin_raw("command").await.unwrap().is_none());

    let scope = ConsoleScopeAllocator::default().scope("raw");
    let scoped = ConsoleTarget::new(console, 1, Some(scope));
    assert!(scoped.begin_raw("command").await.unwrap().is_some());
  }
}
