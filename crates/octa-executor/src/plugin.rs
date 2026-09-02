//! Shared plugin invocation used by task commands and value evaluation.

use std::{collections::HashMap, io, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use octa_plugin::{
  logger::{collect_value_redactions, redact},
  protocol::PluginResponse,
};
use octa_plugin_manager::plugin_client::PluginExecutionRequest;
use octa_plugin_manager::plugin_manager::PluginManager;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::error::{ExecutorError, ExecutorResult};

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
  pub silent: bool,
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
        },
        cancel_token.clone(),
      )
      .await
      .map_err(io::Error::from)?;
    let command_id = execution.id().to_owned();

    let mut output = PluginOutput {
      code: -1,
      stdout: String::new(),
      stderr: String::new(),
    };
    loop {
      match execution.receive_output(&cancel_token).await {
        Ok(Some(response)) => match response {
          PluginResponse::Stdout { id, line } if id == command_id => {
            if !request.silent {
              println!("{}", line.trim());
            }
            output.stdout.push_str(line.trim());
            output.stdout.push('\n');
          },
          PluginResponse::Stderr { id, line } if id == command_id => {
            if !request.silent {
              eprintln!("{}", line.trim());
            }
            output.stderr.push_str(line.trim());
            output.stderr.push('\n');
          },
          PluginResponse::ExitStatus { id, code } if id == command_id => {
            output.code = code;
            return Ok(output);
          },
          PluginResponse::Error { id, message } if id == command_id => {
            return Err(io::Error::other(format!("Plugin error: {message}")).into());
          },
          _ => {},
        },
        Ok(None) => {
          return Err(
            io::Error::new(
              io::ErrorKind::ConnectionAborted,
              "Plugin connection closed unexpectedly",
            )
            .into(),
          );
        },
        Err(error) => {
          if cancel_token.is_cancelled() {
            let _ = execution.cancel_and_wait().await;
            return Err(io::Error::new(io::ErrorKind::Interrupted, "Command cancelled").into());
          }

          return Err(io::Error::from(error).into());
        },
      }
    }
  }
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
          silent: true,
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
