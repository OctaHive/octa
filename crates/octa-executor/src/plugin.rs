//! Shared plugin invocation used by task commands and value evaluation.

use std::{collections::HashMap, io, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use octa_output::ConsoleStream;
use octa_plugin::{
  logger::{collect_value_redactions, redact},
  protocol::{DiagnosticLevel as PluginDiagnosticLevel, PluginResponse},
};
use octa_plugin_manager::plugin_client::PluginExecutionRequest;
use octa_plugin_manager::plugin_manager::PluginManager;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
  console_target::ConsoleTarget,
  error::{ExecutorError, ExecutorResult},
  output_capture::{CaptureError, OutputCapture, MAX_CAPTURED_OUTPUT_BYTES},
  raw_terminal::RawTerminalBridge,
};

#[derive(Clone)]
pub(crate) enum PluginTarget {
  Key(String),
  Capability(String),
}

impl PluginTarget {
  pub(crate) fn name(&self) -> &str {
    match self {
      Self::Key(key) | Self::Capability(key) => key,
    }
  }
}

#[derive(Clone)]
pub(crate) struct PluginInvoker {
  manager: Arc<PluginManager>,
}

pub(crate) struct PluginRequest {
  pub target: PluginTarget,
  pub value: Value,
  pub args: Vec<String>,
  pub context: PluginExecutionContext,
  pub output: Option<ConsoleTarget>,
  pub raw: bool,
}

/// Data inherited by every plugin invocation, regardless of where it originated.
#[derive(Clone)]
pub(crate) struct PluginExecutionContext {
  pub dir: PathBuf,
  pub vars: HashMap<String, Value>,
  pub envs: HashMap<String, String>,
  pub secret_vars: Vec<String>,
  pub dry: bool,
  pub redact_params: bool,
}

pub(crate) struct PluginOutput {
  pub code: i32,
  pub stdout: String,
  pub stderr: String,
}

impl PluginInvoker {
  pub(crate) fn new(manager: Arc<PluginManager>) -> Self {
    Self { manager }
  }

  pub(crate) async fn invoke(
    &self,
    request: PluginRequest,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<PluginOutput> {
    let plugin_name = request.target.name().to_owned();
    let registration = match &request.target {
      PluginTarget::Key(key) => self.manager.resolve_key(key).await,
      PluginTarget::Capability(capability) => {
        match self.manager.resolve_capability(capability).await {
          Some(registration) => Some(registration),
          // Before capabilities were added, conventional task keys represented the same behavior.
          None => self.manager.resolve_key(capability).await,
        }
      },
    }
    .ok_or_else(|| ExecutorError::PluginUnavailable(request.target.name().to_owned()))?;
    registration
      .validate(&request.value)
      .map_err(|error| ExecutorError::PluginValidationFailed(request.target.name().to_owned(), error))?;
    if request.raw && !registration.supports_raw() {
      return Err(ExecutorError::RawUnsupported(request.target.name().to_owned()));
    }
    let _execution_guard = self.manager.execution_guard(request.raw).await;

    let PluginExecutionContext {
      dir,
      vars,
      envs,
      secret_vars,
      dry,
      redact_params,
    } = request.context;

    let client = self
      .manager
      .get_client(registration.plugin_name())
      .await
      .map_err(|error| io::Error::new(io::ErrorKind::NotFound, error.to_string()))?;

    let command = match request.value {
      Value::String(command) => command,
      value => value.to_string(),
    };
    let mut execution = client
      .start_execution(
        PluginExecutionRequest {
          params: command,
          dry,
          args: request.args,
          dir,
          vars,
          envs,
          secret_vars,
          redact_params,
          raw: request.raw,
        },
        cancel_token.clone(),
      )
      .await
      .map_err(io::Error::from)?;
    let command_id = execution.id().to_owned();
    let mut raw_session = if request.raw {
      match &request.output {
        Some(output) => match output.begin_raw(&command_id).await {
          Ok(session) => session,
          Err(error) => {
            let _ = execution.cancel_and_wait().await;
            return Err(error.into());
          },
        },
        None => None,
      }
    } else {
      None
    };
    let mut terminal_bridge = if request.raw {
      match RawTerminalBridge::start(execution.terminal_input()).await {
        Ok(bridge) => Some(bridge),
        Err(error) => {
          drop(raw_session);
          let _ = execution.cancel_and_wait().await;
          return Err(error.into());
        },
      }
    } else {
      None
    };

    let mut terminal_response = false;
    let result: ExecutorResult<PluginOutput> = async {
      let mut output = OutputCapture::default();
      loop {
        match execution.receive_output(&cancel_token).await {
          Ok(Some(response)) => match response {
            PluginResponse::Stdout { id, line } if id == command_id => {
              let line = without_line_ending(&line);
              if let Some(session) = &mut raw_session {
                if !request
                  .output
                  .as_ref()
                  .is_some_and(|target| target.hides(ConsoleStream::Stdout))
                {
                  let mut bytes = line.as_bytes().to_vec();
                  bytes.push(b'\n');
                  session.write(ConsoleStream::Stdout, bytes).await?;
                }
              } else if let Some(target) = &request.output {
                target.line(&command_id, ConsoleStream::Stdout, line.to_owned()).await?;
              }
              output
                .append_line(ConsoleStream::Stdout, line)
                .await
                .map_err(|error| capture_error(&plugin_name, error))?;
            },
            PluginResponse::Stderr { id, line } if id == command_id => {
              let line = without_line_ending(&line);
              if let Some(session) = &mut raw_session {
                if !request
                  .output
                  .as_ref()
                  .is_some_and(|target| target.hides(ConsoleStream::Stderr))
                {
                  let mut bytes = line.as_bytes().to_vec();
                  bytes.push(b'\n');
                  session.write(ConsoleStream::Stderr, bytes).await?;
                }
              } else if let Some(target) = &request.output {
                target.line(&command_id, ConsoleStream::Stderr, line.to_owned()).await?;
              }
              output
                .append_line(ConsoleStream::Stderr, line)
                .await
                .map_err(|error| capture_error(&plugin_name, error))?;
            },
            PluginResponse::StdoutBytes { id, bytes } if id == command_id => {
              if let Some(session) = &mut raw_session {
                if !request
                  .output
                  .as_ref()
                  .is_some_and(|target| target.hides(ConsoleStream::Stdout))
                {
                  session.write(ConsoleStream::Stdout, bytes.clone()).await?;
                }
              } else if let Some(target) = &request.output {
                target.bytes(&command_id, ConsoleStream::Stdout, bytes.clone()).await?;
              }
              output
                .append(ConsoleStream::Stdout, String::from_utf8_lossy(&bytes).as_bytes())
                .await
                .map_err(|error| capture_error(&plugin_name, error))?;
            },
            PluginResponse::StderrBytes { id, bytes } if id == command_id => {
              if let Some(session) = &mut raw_session {
                if !request
                  .output
                  .as_ref()
                  .is_some_and(|target| target.hides(ConsoleStream::Stderr))
                {
                  session.write(ConsoleStream::Stderr, bytes.clone()).await?;
                }
              } else if let Some(target) = &request.output {
                target.bytes(&command_id, ConsoleStream::Stderr, bytes.clone()).await?;
              }
              output
                .append(ConsoleStream::Stderr, String::from_utf8_lossy(&bytes).as_bytes())
                .await
                .map_err(|error| capture_error(&plugin_name, error))?;
            },
            PluginResponse::Diagnostic {
              id,
              level,
              message,
              location,
            } if id == command_id => {
              if let Some(target) = &request.output {
                let level = match level {
                  PluginDiagnosticLevel::Trace => octa_output::ConsoleLevel::Trace,
                  PluginDiagnosticLevel::Debug => octa_output::ConsoleLevel::Debug,
                  PluginDiagnosticLevel::Info => octa_output::ConsoleLevel::Info,
                  PluginDiagnosticLevel::Warn => octa_output::ConsoleLevel::Warn,
                  PluginDiagnosticLevel::Error => octa_output::ConsoleLevel::Error,
                };
                let location = location.map(|location| octa_output::SourceLocation {
                  file: location.file,
                  line: location.line,
                  column: location.column,
                });
                target.diagnostic(level, message, location).await?;
              }
            },
            PluginResponse::ExitStatus { id, code } if id == command_id => {
              terminal_response = true;
              let (stdout, stderr) = output.into_strings().await?;
              break Ok(PluginOutput { code, stdout, stderr });
            },
            PluginResponse::Error { id, message } if id == command_id => {
              terminal_response = true;
              break Err(io::Error::other(format!("Plugin error: {message}")).into());
            },
            _ => {},
          },
          Ok(None) => {
            break Err(
              io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Plugin connection closed unexpectedly",
              )
              .into(),
            );
          },
          Err(error) => {
            if cancel_token.is_cancelled() {
              break Err(io::Error::new(io::ErrorKind::Interrupted, "Command cancelled").into());
            }

            break Err(io::Error::from(error).into());
          },
        }
      }
    }
    .await;
    if result.is_err() && !terminal_response {
      let _ = execution.cancel_and_wait().await;
    }
    if let Some(bridge) = &mut terminal_bridge {
      bridge.shutdown().await;
    }
    drop(raw_session);
    result
  }
}

fn capture_error(plugin: &str, error: CaptureError) -> ExecutorError {
  match error {
    CaptureError::LimitExceeded => ExecutorError::PluginOutputTooLarge {
      plugin: plugin.to_owned(),
      limit_mib: MAX_CAPTURED_OUTPUT_BYTES / (1024 * 1024),
    },
    CaptureError::Io(error) => error.into(),
  }
}

fn without_line_ending(line: &str) -> &str {
  let line = line.strip_suffix('\n').unwrap_or(line);
  line.strip_suffix('\r').unwrap_or(line)
}

/// Runtime context shared by structured plugin values and Terra plugin helpers.
#[derive(Clone)]
pub(crate) struct EvaluationRequest {
  pub target: PluginTarget,
  pub value: Value,
  pub context: PluginExecutionContext,
}

#[async_trait]
pub(crate) trait PluginEvaluator: Send + Sync {
  async fn evaluate(&self, request: EvaluationRequest, cancel_token: CancellationToken) -> ExecutorResult<String>;
}

#[derive(Clone)]
pub(crate) struct ManagerPluginEvaluator {
  invoker: PluginInvoker,
}

impl ManagerPluginEvaluator {
  pub(crate) fn new(manager: Arc<PluginManager>) -> Self {
    Self {
      invoker: PluginInvoker::new(manager),
    }
  }
}

#[async_trait]
impl PluginEvaluator for ManagerPluginEvaluator {
  async fn evaluate(&self, request: EvaluationRequest, cancel_token: CancellationToken) -> ExecutorResult<String> {
    let key = request.target.name().to_owned();
    let mut redactions = Vec::new();
    for value in request
      .context
      .secret_vars
      .iter()
      .filter_map(|name| request.context.vars.get(name))
    {
      collect_value_redactions(value, &mut redactions);
    }
    let output = self
      .invoker
      .invoke(
        PluginRequest {
          target: request.target,
          value: request.value,
          args: Vec::new(),
          context: request.context,
          output: None,
          raw: false,
        },
        cancel_token,
      )
      .await?;

    if output.code == 0 {
      Ok(output.stdout.trim().to_owned())
    } else {
      Err(ExecutorError::PluginEvaluationFailed {
        key,
        code: output.code,
        stderr: redact(output.stderr.trim(), &redactions),
      })
    }
  }
}

#[cfg(test)]
pub(crate) struct SystemTestEvaluator;

#[cfg(test)]
#[async_trait]
impl PluginEvaluator for SystemTestEvaluator {
  async fn evaluate(&self, request: EvaluationRequest, _cancel_token: CancellationToken) -> ExecutorResult<String> {
    use std::process::Stdio;

    if request.context.dry {
      return Ok(String::new());
    }
    let command = request.value.as_str().ok_or_else(|| {
      ExecutorError::PluginValidationFailed(request.target.name().to_owned(), "expected a string".to_owned())
    })?;

    #[cfg(windows)]
    let mut child = {
      let mut child = tokio::process::Command::new("cmd");
      child.args(["/C", command]);
      child
    };
    #[cfg(not(windows))]
    let mut child = {
      let mut child = tokio::process::Command::new("sh");
      child.args(["-c", command]);
      child
    };
    let output = child
      .current_dir(&request.context.dir)
      .envs(&request.context.envs)
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .output()
      .await?;
    if output.status.success() {
      return Ok(String::from_utf8_lossy(output.stdout.trim_ascii_end()).into_owned());
    }

    let mut redactions = Vec::new();
    for value in request
      .context
      .secret_vars
      .iter()
      .filter_map(|name| request.context.vars.get(name))
    {
      collect_value_redactions(value, &mut redactions);
    }
    Err(ExecutorError::PluginEvaluationFailed {
      key: request.target.name().to_owned(),
      code: output.status.code().unwrap_or(-1),
      stderr: redact(&String::from_utf8_lossy(&output.stderr), &redactions),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::without_line_ending;

  #[test]
  fn removes_protocol_line_endings_without_trimming_output() {
    assert_eq!(without_line_ending("  value  \r\n"), "  value  ");
    assert_eq!(without_line_ending("  value  "), "  value  ");
    assert_eq!(without_line_ending("\n"), "");
  }
}
