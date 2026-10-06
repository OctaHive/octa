//! Command-scoped coordination of one Codex invocation.
//!
//! This is the single owner of the child lifecycle after compatibility and
//! task input have been validated. It arbitrates process exit against the SDK
//! cancellation token, writes the prompt, drains both output pipes, and emits
//! only non-terminal activity. The plugin entry point remains the sole place
//! allowed to send `Completed` or `Error`, so exit/cancel races cannot produce
//! two terminal protocol responses.

use std::{io, path::Path, process::ExitStatus, sync::Arc, time::Duration};

use anyhow::Context;
use octa_plugin::{protocol::PluginResponse, send_response};
use tokio::{
  io::{AsyncReadExt, AsyncWrite, AsyncWriteExt},
  sync::Mutex,
  task::{JoinError, JoinHandle},
  time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::{
  events::{EventDecoder, HarnessEvent},
  invocation::CodexInvocation,
  process::ProcessTree,
  records::TraceWriter,
  sanitization::{RunSanitizer, SanitizedEvent},
};

const STREAM_BUFFER_BYTES: usize = 16 * 1024;
/// Time allowed for stdin closure and cooperative process termination.
const CANCELLATION_GRACE_PERIOD: Duration = Duration::from_secs(2);
/// Bounds pipe draining after the owned process tree has stopped.
const POST_TERMINATION_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounds reaping after the force-kill request has been issued.
const FORCE_TERMINATION_TIMEOUT: Duration = Duration::from_secs(2);

/// The single terminal decision returned to the plugin entry point.
pub(crate) enum CommandOutcome {
  /// The SDK cancellation token won the lifecycle race.
  Cancelled,
  /// The process and both output streams ended normally at the OS layer.
  Completed(CommandCompletion),
}

/// Evidence retained after a complete, well-formed harness stream.
pub(crate) struct CommandCompletion {
  terminal_event: SanitizedEvent,
}

/// References shared by event handling during one command invocation.
struct CommandContext<'a, W> {
  command_id: &'a str,
  writer: Arc<Mutex<W>>,
  sanitizer: &'a RunSanitizer,
  trace: &'a mut TraceWriter,
  cancellation: &'a CancellationToken,
}

impl CommandCompletion {
  /// Sanitized terminal event consumed later by result normalization.
  pub(crate) fn terminal_event(&self) -> &SanitizedEvent {
    &self.terminal_event
  }
}

/// Runs one prepared invocation and forwards ordered, non-terminal activity.
pub(crate) async fn run<W>(
  command_id: &str,
  invocation: &CodexInvocation,
  working_directory: &Path,
  writer: Arc<Mutex<W>>,
  sanitizer: &RunSanitizer,
  trace: &mut TraceWriter,
  cancellation: &CancellationToken,
) -> anyhow::Result<CommandOutcome>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  let context = CommandContext {
    command_id,
    writer,
    sanitizer,
    trace,
    cancellation,
  };
  run_with_grace_period(invocation, working_directory, context, CANCELLATION_GRACE_PERIOD).await
}

async fn run_with_grace_period<W>(
  invocation: &CodexInvocation,
  working_directory: &Path,
  mut context: CommandContext<'_, W>,
  grace_period: Duration,
) -> anyhow::Result<CommandOutcome>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  if context.cancellation.is_cancelled() {
    return Ok(CommandOutcome::Cancelled);
  }

  let mut command = invocation.command(working_directory).await?;
  // Fingerprinting may outlive a cancellation request, but process creation
  // must not. Recheck immediately at the irreversible spawn boundary.
  if context.cancellation.is_cancelled() {
    return Ok(CommandOutcome::Cancelled);
  }

  let mut tree = ProcessTree::spawn(&mut command).context("failed to start the Codex process")?;
  let stdin = tree.take_stdin().context("failed to capture Codex stdin")?;
  let mut stdout = tree.take_stdout().context("failed to capture Codex stdout")?;
  let mut stderr = tree.take_stderr().context("failed to capture Codex stderr")?;
  let mut prompt = PromptWriter::spawn(stdin, invocation.prompt_bytes().to_vec());

  let mut decoder = Some(EventDecoder::new());
  let mut terminal_event = None;
  let mut stdout_closed = false;
  let mut stderr_closed = false;
  let mut prompt_closed = false;
  let mut exit_status = None;
  let mut stdout_buffer = vec![0_u8; STREAM_BUFFER_BYTES];
  let mut stderr_buffer = vec![0_u8; STREAM_BUFFER_BYTES];
  let drain_deadline = tokio::time::sleep(Duration::MAX);
  tokio::pin!(drain_deadline);

  loop {
    if exit_status.is_some() && stdout_closed && stderr_closed && prompt_closed {
      break;
    }

    let step = tokio::select! {
      biased;
      _ = context.cancellation.cancelled() => LifecycleStep::Cancelled,
      result = prompt.wait(), if !prompt_closed => LifecycleStep::Prompt(result),
      result = stdout.read(&mut stdout_buffer), if !stdout_closed => LifecycleStep::Stdout(result),
      result = stderr.read(&mut stderr_buffer), if !stderr_closed => LifecycleStep::Stderr(result),
      result = tree.wait(), if exit_status.is_none() => LifecycleStep::Exited(result),
      _ = &mut drain_deadline, if exit_status.is_some() => LifecycleStep::DrainTimedOut,
    };

    let result: anyhow::Result<()> = match step {
      LifecycleStep::Cancelled => {
        return finish_cancellation(&mut prompt, &mut tree, &mut stdout, &mut stderr, grace_period).await;
      },
      LifecycleStep::Prompt(result) => {
        prompt_closed = true;
        result.context("failed to deliver the Codex prompt")
      },
      LifecycleStep::Stdout(Ok(0)) => {
        stdout_closed = true;
        match finish_stdout(&mut decoder, &mut context, &mut terminal_event).await {
          Ok(ActivityFlow::Continue) => Ok(()),
          Ok(ActivityFlow::Cancelled) => {
            return finish_cancellation(&mut prompt, &mut tree, &mut stdout, &mut stderr, grace_period).await;
          },
          Err(error) => Err(error),
        }
      },
      LifecycleStep::Stdout(Ok(read)) => {
        match process_stdout(&mut decoder, &stdout_buffer[..read], &mut context, &mut terminal_event).await {
          Ok(ActivityFlow::Continue) => Ok(()),
          Ok(ActivityFlow::Cancelled) => {
            return finish_cancellation(&mut prompt, &mut tree, &mut stdout, &mut stderr, grace_period).await;
          },
          Err(error) => Err(error),
        }
      },
      LifecycleStep::Stdout(Err(error)) => Err(error).context("failed to read Codex stdout"),
      LifecycleStep::Stderr(Ok(0)) => {
        stderr_closed = true;
        Ok(())
      },
      // Stderr forwarding and retention have their own contract. This stage
      // drains it so a full pipe cannot deadlock lifecycle termination.
      LifecycleStep::Stderr(Ok(_)) => Ok(()),
      LifecycleStep::Stderr(Err(error)) => Err(error).context("failed to read Codex stderr"),
      LifecycleStep::Exited(Ok(status)) => {
        exit_status = Some(status);
        drain_deadline
          .as_mut()
          .reset(Instant::now() + POST_TERMINATION_DRAIN_TIMEOUT);
        Ok(())
      },
      LifecycleStep::Exited(Err(error)) => Err(error).context("failed to wait for the Codex process"),
      LifecycleStep::DrainTimedOut => Err(anyhow::anyhow!(
        "Codex output pipes did not close within {} seconds after process termination",
        POST_TERMINATION_DRAIN_TIMEOUT.as_secs()
      )),
    };

    if let Err(error) = result {
      prompt.abort_and_wait().await?;
      force_and_drain(&mut tree, &mut stdout, &mut stderr).await?;
      return Err(error);
    }
  }

  let terminal_event = terminal_event.ok_or_else(|| anyhow::anyhow!("Codex stdout ended without a terminal event"))?;
  debug_assert!(
    exit_status.is_some(),
    "the lifecycle loop exits only after process exit"
  );
  Ok(CommandOutcome::Completed(CommandCompletion { terminal_event }))
}

enum LifecycleStep {
  Cancelled,
  Prompt(io::Result<()>),
  Stdout(io::Result<usize>),
  Stderr(io::Result<usize>),
  Exited(io::Result<ExitStatus>),
  DrainTimedOut,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActivityFlow {
  Continue,
  Cancelled,
}

async fn process_stdout<W>(
  decoder: &mut Option<EventDecoder>,
  mut bytes: &[u8],
  context: &mut CommandContext<'_, W>,
  terminal_event: &mut Option<SanitizedEvent>,
) -> anyhow::Result<ActivityFlow>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  while !bytes.is_empty() {
    let Some(event) = decoder
      .as_mut()
      .expect("stdout decoder exists before EOF")
      .next_event(&mut bytes)
      .context("invalid Codex stdout stream")?
    else {
      continue;
    };
    if retain_event(event, context, terminal_event).await? == ActivityFlow::Cancelled {
      return Ok(ActivityFlow::Cancelled);
    }
  }
  Ok(ActivityFlow::Continue)
}

async fn finish_stdout<W>(
  decoder: &mut Option<EventDecoder>,
  context: &mut CommandContext<'_, W>,
  terminal_event: &mut Option<SanitizedEvent>,
) -> anyhow::Result<ActivityFlow>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  let event = decoder
    .take()
    .expect("stdout decoder exists until EOF")
    .finish()
    .context("invalid Codex stdout stream")?;
  if let Some(event) = event {
    return retain_event(event, context, terminal_event).await;
  }
  Ok(ActivityFlow::Continue)
}

async fn retain_event<W>(
  event: HarnessEvent,
  context: &mut CommandContext<'_, W>,
  terminal_event: &mut Option<SanitizedEvent>,
) -> anyhow::Result<ActivityFlow>
where
  W: AsyncWrite + Send + Unpin + 'static,
{
  if context.cancellation.is_cancelled() {
    return Ok(ActivityFlow::Cancelled);
  }
  let event = context.sanitizer.sanitize_event(event)?;
  context.trace.append(&event).await?;
  for response in crate::events::normalize_activity(context.command_id, &event)? {
    if context.cancellation.is_cancelled() {
      return Ok(ActivityFlow::Cancelled);
    }
    debug_assert!(!is_terminal_response(&response));
    send_response(&context.writer, &response).await?;
  }
  if context.cancellation.is_cancelled() {
    return Ok(ActivityFlow::Cancelled);
  }
  if event.terminal().is_some() {
    *terminal_event = Some(event);
  }
  Ok(ActivityFlow::Continue)
}

fn is_terminal_response(response: &PluginResponse) -> bool {
  matches!(
    response,
    PluginResponse::Completed { .. } | PluginResponse::Error { .. }
  )
}

async fn finish_cancellation(
  prompt: &mut PromptWriter,
  tree: &mut ProcessTree,
  stdout: &mut (impl tokio::io::AsyncRead + Unpin),
  stderr: &mut (impl tokio::io::AsyncRead + Unpin),
  grace_period: Duration,
) -> anyhow::Result<CommandOutcome> {
  prompt.abort_and_wait().await?;
  cancel_and_drain(tree, stdout, stderr, grace_period).await?;
  Ok(CommandOutcome::Cancelled)
}

async fn cancel_and_drain(
  tree: &mut ProcessTree,
  stdout: &mut (impl tokio::io::AsyncRead + Unpin),
  stderr: &mut (impl tokio::io::AsyncRead + Unpin),
  grace_period: Duration,
) -> anyhow::Result<()> {
  let graceful = tree
    .request_graceful_termination()
    .context("failed to request graceful Codex termination");
  let force = if graceful.is_ok() {
    match tokio::time::timeout(grace_period, tree.wait()).await {
      Ok(Ok(_)) => None,
      Ok(Err(_)) | Err(_) => Some(force_terminate(tree).await),
    }
  } else {
    Some(force_terminate(tree).await)
  };
  let drain = drain_pipes(stdout, stderr).await;

  // Teardown is best-effort as a whole: a failed cooperative signal must not
  // skip force-kill, and a failed kill must not leave pipe draining unbounded.
  graceful?;
  if let Some(force) = force {
    force?;
  }
  drain
}

async fn force_and_drain(
  tree: &mut ProcessTree,
  stdout: &mut (impl tokio::io::AsyncRead + Unpin),
  stderr: &mut (impl tokio::io::AsyncRead + Unpin),
) -> anyhow::Result<()> {
  let termination = force_terminate(tree).await;
  let drain = drain_pipes(stdout, stderr).await;
  termination?;
  drain
}

async fn force_terminate(tree: &mut ProcessTree) -> anyhow::Result<()> {
  tokio::time::timeout(FORCE_TERMINATION_TIMEOUT, tree.terminate())
    .await
    .map_err(|_| anyhow::anyhow!("timed out while force-terminating the Codex process tree"))?
    .context("failed to force-terminate the Codex process tree")
}

async fn drain_pipes(
  stdout: &mut (impl tokio::io::AsyncRead + Unpin),
  stderr: &mut (impl tokio::io::AsyncRead + Unpin),
) -> anyhow::Result<()> {
  let drain = async {
    let mut stdout_sink = tokio::io::sink();
    let mut stderr_sink = tokio::io::sink();
    let (stdout, stderr) = tokio::join!(
      tokio::io::copy(stdout, &mut stdout_sink),
      tokio::io::copy(stderr, &mut stderr_sink)
    );
    stdout?;
    stderr?;
    io::Result::Ok(())
  };
  tokio::time::timeout(POST_TERMINATION_DRAIN_TIMEOUT, drain)
    .await
    .map_err(|_| anyhow::anyhow!("Codex output pipes did not close after process termination"))??;
  Ok(())
}

/// Abort-on-drop owner for the only task allowed to hold child stdin.
struct PromptWriter {
  task: JoinHandle<io::Result<()>>,
  finished: bool,
}

impl PromptWriter {
  fn spawn(mut stdin: tokio::process::ChildStdin, prompt: Vec<u8>) -> Self {
    let task = tokio::spawn(async move {
      stdin.write_all(&prompt).await?;
      stdin.shutdown().await
    });
    Self { task, finished: false }
  }

  async fn wait(&mut self) -> io::Result<()> {
    let result = (&mut self.task).await;
    self.finished = true;
    join_prompt(result)
  }

  async fn abort_and_wait(&mut self) -> io::Result<()> {
    if self.finished {
      return Ok(());
    }
    self.task.abort();
    let result = (&mut self.task).await;
    self.finished = true;
    match result {
      Err(error) if error.is_cancelled() => Ok(()),
      other => join_prompt(other),
    }
  }
}

impl Drop for PromptWriter {
  fn drop(&mut self) {
    if !self.finished {
      self.task.abort();
    }
  }
}

fn join_prompt(result: Result<io::Result<()>, JoinError>) -> io::Result<()> {
  result.map_err(|error| io::Error::other(format!("Codex prompt writer task failed: {error}")))?
}

#[cfg(test)]
mod tests {
  use std::{
    collections::HashMap,
    future::pending,
    pin::Pin,
    task::{Context, Poll},
  };

  use serde_json::json;
  use tokio::io::AsyncReadExt;

  use super::*;
  use crate::{
    config::CodexConfig,
    invocation::{CodexExecutable, EnvironmentSources, StructuredResultTarget},
    records::RunRecords,
  };

  struct CancelOnWrite {
    cancellation: CancellationToken,
  }

  impl AsyncWrite for CancelOnWrite {
    fn poll_write(self: Pin<&mut Self>, _context: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
      self.cancellation.cancel();
      Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
      Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
      Poll::Ready(Ok(()))
    }
  }

  async fn test_invocation(workspace: &Path) -> CodexInvocation {
    let config = CodexConfig::parse(json!({"prompt": "fixture prompt"})).unwrap();
    let variables = HashMap::new();
    let task_environment = HashMap::new();
    CodexInvocation::load(
      CodexExecutable::fixture(),
      &config,
      workspace,
      StructuredResultTarget::Disabled,
      EnvironmentSources {
        variables: &variables,
        secret_variables: &[],
        task_environment: &task_environment,
      },
    )
    .await
    .unwrap()
  }

  fn test_sanitizer() -> RunSanitizer {
    RunSanitizer::from_variables(&HashMap::new(), &[])
  }

  async fn test_records(workspace: &Path) -> RunRecords {
    RunRecords::create(workspace, ".octa/test-codex-runs", "command")
      .await
      .unwrap()
  }

  #[tokio::test]
  async fn cancelled_activity_is_not_written_or_retained() {
    let workspace = tempfile::tempdir().unwrap();
    let mut records = test_records(workspace.path()).await;
    let mut decoder = EventDecoder::new();
    let mut bytes = br#"{"type":"item.completed","item":{"type":"agent_message","text":"late"}}
"#
    .as_slice();
    let event = decoder.next_event(&mut bytes).unwrap().unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let sanitizer = test_sanitizer();
    let (writer, mut reader) = tokio::io::duplex(256);
    let writer = Arc::new(Mutex::new(writer));
    let mut terminal = None;
    let mut context = CommandContext {
      command_id: "command",
      writer,
      sanitizer: &sanitizer,
      trace: records.trace(),
      cancellation: &cancellation,
    };

    let flow = retain_event(event, &mut context, &mut terminal).await.unwrap();
    drop(context);

    assert_eq!(flow, ActivityFlow::Cancelled);
    assert!(terminal.is_none());
    let mut emitted = Vec::new();
    reader.read_to_end(&mut emitted).await.unwrap();
    assert!(emitted.is_empty(), "activity was emitted after cancellation");
  }

  #[tokio::test]
  async fn stdout_helpers_cover_partial_terminal_and_mid_delivery_cancellation() {
    let workspace = tempfile::tempdir().unwrap();
    let mut records = test_records(workspace.path()).await;
    let mut decoder = Some(EventDecoder::new());
    let (writer, _reader) = tokio::io::duplex(512);
    let writer = Arc::new(Mutex::new(writer));
    let cancellation = CancellationToken::new();
    let sanitizer = test_sanitizer();
    let mut terminal = None;
    let mut context = CommandContext {
      command_id: "command",
      writer,
      sanitizer: &sanitizer,
      trace: records.trace(),
      cancellation: &cancellation,
    };

    let flow = process_stdout(&mut decoder, br#"{"type":"turn.started""#, &mut context, &mut terminal)
      .await
      .unwrap();
    assert_eq!(flow, ActivityFlow::Continue);

    let cancellation = CancellationToken::new();
    let writer = Arc::new(Mutex::new(CancelOnWrite {
      cancellation: cancellation.clone(),
    }));
    let mut decoder = Some(EventDecoder::new());
    let mut terminal = None;
    let sanitizer = test_sanitizer();
    let mut context = CommandContext {
      command_id: "command",
      writer,
      sanitizer: &sanitizer,
      trace: records.trace(),
      cancellation: &cancellation,
    };
    let flow = process_stdout(
      &mut decoder,
      br#"{"type":"item.completed","item":{"type":"agent_message","text":"done"}}
"#,
      &mut context,
      &mut terminal,
    )
    .await
    .unwrap();
    assert_eq!(flow, ActivityFlow::Cancelled);

    let mut shutdown_writer = CancelOnWrite {
      cancellation: CancellationToken::new(),
    };
    shutdown_writer.shutdown().await.unwrap();

    let cancellation = CancellationToken::new();
    let writer = Arc::new(Mutex::new(CancelOnWrite {
      cancellation: cancellation.clone(),
    }));
    let mut decoder = Some(EventDecoder::new());
    let mut terminal = None;
    let sanitizer = test_sanitizer();
    let mut context = CommandContext {
      command_id: "command",
      writer,
      sanitizer: &sanitizer,
      trace: records.trace(),
      cancellation: &cancellation,
    };
    let flow = process_stdout(
      &mut decoder,
      br#"{"type":"item.completed","item":{"type":"command_execution","aggregated_output":"failed","exit_code":1,"status":"failed"}}
"#,
      &mut context,
      &mut terminal,
    )
    .await
    .unwrap();
    assert_eq!(flow, ActivityFlow::Cancelled);

    let (writer, _reader) = tokio::io::duplex(512);
    let mut decoder = Some(EventDecoder::new());
    decoder
      .as_mut()
      .unwrap()
      .next_event(&mut br#"{"type":"turn.completed","message":"done"}"#.as_slice())
      .unwrap();
    let mut terminal = None;
    let sanitizer = test_sanitizer();
    let cancellation = CancellationToken::new();
    let mut context = CommandContext {
      command_id: "command",
      writer: Arc::new(Mutex::new(writer)),
      sanitizer: &sanitizer,
      trace: records.trace(),
      cancellation: &cancellation,
    };
    let flow = finish_stdout(&mut decoder, &mut context, &mut terminal).await.unwrap();
    assert_eq!(flow, ActivityFlow::Continue);
    assert!(terminal.is_some());
  }

  #[tokio::test]
  async fn cancelled_and_invalid_invocations_finish_without_leaking_process_state() {
    let workspace = tempfile::tempdir().unwrap();
    let invocation = test_invocation(workspace.path()).await;
    let mut records = test_records(workspace.path()).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let (writer, _reader) = tokio::io::duplex(512);
    let sanitizer = test_sanitizer();
    let context = CommandContext {
      command_id: "command",
      writer: Arc::new(Mutex::new(writer)),
      sanitizer: &sanitizer,
      trace: records.trace(),
      cancellation: &cancellation,
    };
    let outcome = run_with_grace_period(&invocation, workspace.path(), context, Duration::from_millis(20))
      .await
      .unwrap();
    assert!(matches!(outcome, CommandOutcome::Cancelled));

    let (writer, _reader) = tokio::io::duplex(512);
    let sanitizer = test_sanitizer();
    let cancellation = CancellationToken::new();
    let context = CommandContext {
      command_id: "command",
      writer: Arc::new(Mutex::new(writer)),
      sanitizer: &sanitizer,
      trace: records.trace(),
      cancellation: &cancellation,
    };
    let error = run_with_grace_period(&invocation, workspace.path(), context, Duration::from_millis(20))
      .await
      .err()
      .expect("invalid harness stream must fail");
    assert!(
      error.to_string().contains("invalid Codex stdout stream"),
      "unexpected lifecycle error: {error:#}"
    );
  }

  #[tokio::test]
  async fn prompt_writer_abort_and_drop_are_idempotent() {
    let mut completed = PromptWriter {
      task: tokio::spawn(async { Ok(()) }),
      finished: false,
    };
    completed.wait().await.unwrap();
    completed.abort_and_wait().await.unwrap();

    let mut pending_writer = PromptWriter {
      task: tokio::spawn(pending::<io::Result<()>>()),
      finished: false,
    };
    pending_writer.abort_and_wait().await.unwrap();

    let dropped = PromptWriter {
      task: tokio::spawn(pending::<io::Result<()>>()),
      finished: false,
    };
    drop(dropped);
  }
}
