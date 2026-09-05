use std::{io, sync::Arc};

use octa_output::{Console, ConsoleLevel, ConsolePayload, ConsoleScope, ConsoleStream, ExecutionEvent};

/// Associates task output and diagnostics with one execution and optional scope.
#[derive(Clone)]
pub(crate) struct ConsoleTarget {
  console: Arc<Console>,
  run_id: u64,
  scope: Option<ConsoleScope>,
}

impl ConsoleTarget {
  pub(crate) fn new(console: Arc<Console>, run_id: u64, scope: Option<ConsoleScope>) -> Self {
    Self { console, run_id, scope }
  }

  pub(crate) async fn line(&self, command_id: &str, stream: ConsoleStream, line: String) -> io::Result<()> {
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

  pub(crate) async fn message(&self, level: ConsoleLevel, message: impl Into<String>) -> io::Result<()> {
    match &self.scope {
      Some(scope) => {
        self
          .console
          .run_message_at(self.run_id, scope.clone(), level, message)
          .await
      },
      None => self.console.run_message(self.run_id, level, message).await,
    }
  }
}
