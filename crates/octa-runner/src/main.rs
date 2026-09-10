use std::{io, process::ExitCode, sync::Arc};

use octa_executor::{ExecutionConclusion, ExecutorError};
use octa_output::Console;
use octa_runner::{
  capabilities, hello, read_frame, MessageWriter, RunRequest, RunStatus, RunnerCommand, RunnerMessage,
  RUNNER_PROTOCOL_VERSION,
};
use octa_runtime::{RunOptions, Runtime, RuntimeConfig};
use tokio::io::{BufReader, Stdin};
use tokio_util::sync::CancellationToken;

const EXIT_INVALID_REQUEST: u8 = 2;
const EXIT_INFRASTRUCTURE: u8 = 3;
const EXIT_PROTOCOL: u8 = 4;

#[tokio::main]
async fn main() -> ExitCode {
  match run().await {
    Ok(RunStatus::Succeeded) => ExitCode::SUCCESS,
    Ok(RunStatus::Failed) => ExitCode::from(1),
    Ok(RunStatus::Cancelled) => ExitCode::from(130),
    Err(code) => ExitCode::from(code),
  }
}

async fn run() -> Result<RunStatus, u8> {
  let output = MessageWriter::default();
  let mut arguments = std::env::args_os().skip(1);
  if arguments.next().as_deref() == Some(std::ffi::OsStr::new("capabilities")) && arguments.next().is_none() {
    emit(&output, &capabilities()).map_err(|_| EXIT_INFRASTRUCTURE)?;
    return Ok(RunStatus::Succeeded);
  }
  if emit(&output, &hello()).is_err() {
    return Err(EXIT_INFRASTRUCTURE);
  }

  let mut input = BufReader::new(tokio::io::stdin());
  let mut first = String::new();
  match read_frame(&mut input, &mut first).await {
    Ok(0) => {
      let _ = emit_error(&output, None, "expected a start command");
      return Err(EXIT_PROTOCOL);
    },
    Ok(_) => {},
    Err(error) => {
      let _ = emit_error(&output, None, &format!("failed to read start command: {error}"));
      return Err(if error.kind() == io::ErrorKind::InvalidData {
        EXIT_PROTOCOL
      } else {
        EXIT_INFRASTRUCTURE
      });
    },
  }
  let command = match serde_json::from_str::<RunnerCommand>(&first) {
    Ok(command) => command,
    Err(error) => {
      let _ = emit_error(&output, None, &format!("invalid start command: {error}"));
      return Err(EXIT_PROTOCOL);
    },
  };
  let RunnerCommand::Start {
    protocol_version,
    request_id,
    request,
  } = command
  else {
    let _ = emit_error(&output, None, "the first command must be start");
    return Err(EXIT_PROTOCOL);
  };
  if protocol_version != RUNNER_PROTOCOL_VERSION {
    let _ = emit_error(
      &output,
      Some(&request_id),
      &format!("unsupported runner protocol version {protocol_version}"),
    );
    return Err(EXIT_PROTOCOL);
  }
  if request_id.is_empty() {
    let _ = emit_error(&output, None, "request_id must not be empty");
    return Err(EXIT_INVALID_REQUEST);
  }
  if let Err(error) = request.validate() {
    let _ = emit_error(&output, Some(&request_id), &error);
    return Err(EXIT_INVALID_REQUEST);
  }
  if emit(
    &output,
    &RunnerMessage::Accepted {
      request_id: &request_id,
    },
  )
  .is_err()
  {
    return Err(EXIT_INFRASTRUCTURE);
  }

  let cancellation = CancellationToken::new();
  let control = tokio::spawn(read_control(
    input,
    request_id.clone(),
    cancellation.clone(),
    output.clone(),
  ));
  install_signal_cancellation(cancellation.clone());

  let console = Arc::new(Console::new(output.event_renderer(request_id.clone())));
  let runtime = match load_runtime(&request, console.clone(), cancellation.clone()).await {
    Ok(runtime) => Arc::new(runtime),
    Err(octa_runtime::RuntimeError::Cancelled) => {
      control.abort();
      let _ = console.drain().await;
      emit(
        &output,
        &RunnerMessage::Finished {
          request_id: &request_id,
          status: RunStatus::Cancelled,
          results: &[],
        },
      )
      .map_err(|_| EXIT_INFRASTRUCTURE)?;
      return Ok(RunStatus::Cancelled);
    },
    Err(error) => {
      control.abort();
      let _ = console.drain().await;
      let _ = emit_error(&output, Some(&request_id), &error.to_string());
      return Err(runtime_error_exit(&error));
    },
  };
  let commands = runtime.qualify_commands(request.commands.clone());
  let options = RunOptions {
    parallel: request.parallel,
    dry: request.dry,
    force: request.force,
    failfast: request.failfast,
    variables: request.variables.clone().into_iter().collect(),
    task_args: request.arguments.clone(),
    quiet: request.quiet,
    silence: request.silence,
    raw: false,
  };

  let result = async {
    let (executions, _) = runtime.prepare(&commands, &options).await?;
    runtime.execute(executions, options.parallel, options.failfast).await
  }
  .await;
  runtime.shutdown().await;
  control.abort();
  if console.drain().await.is_err() {
    return Err(EXIT_INFRASTRUCTURE);
  }

  match result {
    Ok(results) => {
      let status = if results
        .iter()
        .any(|result| matches!(result.conclusion, ExecutionConclusion::Cancelled(_)))
      {
        RunStatus::Cancelled
      } else if results.iter().all(|result| result.is_success()) {
        RunStatus::Succeeded
      } else {
        RunStatus::Failed
      };
      emit(
        &output,
        &RunnerMessage::Finished {
          request_id: &request_id,
          status,
          results: &results,
        },
      )
      .map_err(|_| EXIT_INFRASTRUCTURE)?;
      Ok(status)
    },
    Err(octa_runtime::RuntimeError::Cancelled) => {
      emit(
        &output,
        &RunnerMessage::Finished {
          request_id: &request_id,
          status: RunStatus::Cancelled,
          results: &[],
        },
      )
      .map_err(|_| EXIT_INFRASTRUCTURE)?;
      Ok(RunStatus::Cancelled)
    },
    Err(error) => {
      let _ = emit_error(&output, Some(&request_id), &error.to_string());
      Err(runtime_error_exit(&error))
    },
  }
}

fn runtime_error_exit(error: &octa_runtime::RuntimeError) -> u8 {
  match error {
    octa_runtime::RuntimeError::Io(_) | octa_runtime::RuntimeError::State(_) | octa_runtime::RuntimeError::Join(_) => {
      EXIT_INFRASTRUCTURE
    },
    octa_runtime::RuntimeError::PluginManagerConfiguration(_) => EXIT_INVALID_REQUEST,
    octa_runtime::RuntimeError::PluginInfrastructure(_) => EXIT_INFRASTRUCTURE,
    octa_runtime::RuntimeError::Execution(error) => executor_error_exit(error),
    octa_runtime::RuntimeError::Octafile(_)
    | octa_runtime::RuntimeError::Monorepo(_)
    | octa_runtime::RuntimeError::PluginLock(_)
    | octa_runtime::RuntimeError::PluginConfiguration(_) => EXIT_INVALID_REQUEST,
    octa_runtime::RuntimeError::Cancelled => 130,
  }
}

fn executor_error_exit(error: &ExecutorError) -> u8 {
  match error {
    ExecutorError::ShutdownTimeout
    | ExecutorError::ExecutionIdentityError(_)
    | ExecutorError::OpenFingerprintDbError(_)
    | ExecutorError::FreshnessStateUnavailable(_)
    | ExecutorError::FreshnessStateAlreadyPublished
    | ExecutorError::ChannelError
    | ExecutorError::ConcurrencyLimiterClosed
    | ExecutorError::IoError(_)
    | ExecutorError::JoinError(_) => EXIT_INFRASTRUCTURE,
    _ => EXIT_INVALID_REQUEST,
  }
}

async fn load_runtime(
  request: &RunRequest,
  console: Arc<Console>,
  cancellation: CancellationToken,
) -> Result<Runtime, octa_runtime::RuntimeError> {
  let mut config = RuntimeConfig::headless(
    request.workspace.clone(),
    request.plugins_dir.clone(),
    request.data_dir.clone(),
    console,
  );
  config.octafile = request.octafile.clone();
  config.plugin_lock = request.plugin_lock.clone();
  config.secrets_profile = request.secrets_profile.clone();
  config.plugins = request.plugins.clone();
  config.default_plugin = request.default_plugin.clone();
  config.variables = request.variables.clone().into_iter().collect();
  config.concurrency = request.concurrency;
  config.cancellation = cancellation;
  Runtime::load(config).await
}

async fn read_control(
  mut input: BufReader<Stdin>,
  request_id: String,
  cancellation: CancellationToken,
  output: MessageWriter,
) {
  loop {
    let mut frame = String::new();
    match read_frame(&mut input, &mut frame).await {
      Ok(0) => return,
      Ok(_) => match serde_json::from_str::<RunnerCommand>(&frame) {
        Ok(RunnerCommand::Cancel {
          request_id: cancelled_id,
        }) if cancelled_id == request_id => {
          cancellation.cancel();
          return;
        },
        Ok(RunnerCommand::Cancel {
          request_id: cancelled_id,
        }) => {
          let _ = emit_error(
            &output,
            Some(&request_id),
            &format!("cancel request_id '{cancelled_id}' does not match the active request"),
          );
        },
        Ok(RunnerCommand::Start { .. }) => {
          let _ = emit_error(
            &output,
            Some(&request_id),
            "start may only be sent as the first command",
          );
        },
        Err(error) => {
          let _ = emit_error(&output, Some(&request_id), &format!("invalid control command: {error}"));
        },
      },
      Err(error) => {
        let _ = emit_error(
          &output,
          Some(&request_id),
          &format!("failed to read control command: {error}"),
        );
        cancellation.cancel();
        return;
      },
    }
  }
}

fn install_signal_cancellation(cancellation: CancellationToken) {
  tokio::spawn(async move {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let terminate = async {
      match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(mut signal) => {
          signal.recv().await;
        },
        Err(_) => std::future::pending::<()>().await,
      }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
      _ = ctrl_c => {},
      _ = terminate => {},
    }
    cancellation.cancel();
  });
}

fn emit(output: &MessageWriter, message: &RunnerMessage<'_>) -> io::Result<()> {
  output.write(message)
}

fn emit_error(output: &MessageWriter, request_id: Option<&str>, message: &str) -> io::Result<()> {
  emit(output, &RunnerMessage::Error { request_id, message })
}

#[cfg(test)]
mod tests {
  use octa_plugin_manager::{plugin_lock::PluginLockError, plugin_manager::PluginManagerError};

  use super::*;

  #[test]
  fn classifies_request_and_infrastructure_failures() {
    let invalid = octa_runtime::RuntimeError::Execution(ExecutorError::TaskNotFound("missing".to_owned()));
    assert_eq!(runtime_error_exit(&invalid), EXIT_INVALID_REQUEST);

    let infrastructure = octa_runtime::RuntimeError::Execution(ExecutorError::IoError(io::Error::other("closed")));
    assert_eq!(runtime_error_exit(&infrastructure), EXIT_INFRASTRUCTURE);
    assert_eq!(
      runtime_error_exit(&octa_runtime::RuntimeError::PluginConfiguration("default".to_owned())),
      EXIT_INVALID_REQUEST
    );
    assert_eq!(
      runtime_error_exit(&octa_runtime::RuntimeError::PluginManagerConfiguration(
        PluginManagerError::PluginNotFound("missing".to_owned())
      )),
      EXIT_INVALID_REQUEST
    );
    assert_eq!(
      runtime_error_exit(&octa_runtime::RuntimeError::PluginInfrastructure(
        PluginManagerError::ConnectionError("closed".to_owned())
      )),
      EXIT_INFRASTRUCTURE
    );
    assert_eq!(
      runtime_error_exit(&octa_runtime::RuntimeError::PluginLock(Box::new(
        PluginLockError::MissingPlugin("missing".to_owned()),
      ))),
      EXIT_INVALID_REQUEST
    );
    assert_eq!(
      runtime_error_exit(&octa_runtime::RuntimeError::Io(io::Error::other("closed"))),
      EXIT_INFRASTRUCTURE
    );
    assert_eq!(runtime_error_exit(&octa_runtime::RuntimeError::Cancelled), 130);
  }
}
