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
