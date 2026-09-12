//! Shared plugin invocation used by task commands and value evaluation.

use std::{
  collections::{HashMap, HashSet},
  io,
  path::PathBuf,
  sync::{Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use octa_output::{ConsoleStream, ProgressUpdate, SourceLocation};
use octa_plugin::{
  logger::{collect_value_redactions, redact},
  protocol::{ArtifactDeclaration, DiagnosticLevel as PluginDiagnosticLevel, PluginResponse, ReportDeclaration},
};
use octa_plugin_manager::plugin_client::PluginExecutionRequest;
use octa_plugin_manager::plugin_manager::PluginManager;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::{
  error::{ExecutorError, ExecutorResult},
  output_capture::{CaptureError, OutputCapture, MAX_CAPTURED_OUTPUT_BYTES},
  runtime_output::RuntimeOutput,
  terminal::{RawTerminalConnector, RawTerminalInput, UnsupportedRawTerminal},
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
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
  terminal: Arc<dyn RawTerminalConnector>,
}

pub(crate) struct PluginRequest {
  pub target: PluginTarget,
  pub value: Value,
  pub args: Vec<String>,
  pub context: PluginExecutionContext,
  pub output: Option<RuntimeOutput>,
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
  pub outputs: Map<String, Value>,
  pub artifacts: Vec<ArtifactDeclaration>,
  pub reports: Vec<ReportDeclaration>,
  pub failure_location: Option<SourceLocation>,
}

impl PluginInvoker {
  pub(crate) fn new(manager: Arc<PluginManager>) -> Self {
    Self {
      manager,
      terminal: Arc::new(UnsupportedRawTerminal),
    }
  }

  pub(crate) fn with_terminal(manager: Arc<PluginManager>, terminal: Arc<dyn RawTerminalConnector>) -> Self {
    Self { manager, terminal }
  }

  pub(crate) async fn invoke(
    &self,
    request: PluginRequest,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<PluginOutput> {
    let plugin_name = request.target.name().to_owned();
    let registration = match &request.target {
      PluginTarget::Key(key) => self.manager.resolve_key(key).await,
      PluginTarget::Capability(capability) => self.manager.resolve_capability(capability).await,
    }
    .ok_or_else(|| ExecutorError::PluginUnavailable(request.target.name().to_owned()))?;
    registration
      .validate_input(&request.value)
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
    let mut redactions = Vec::new();
    for value in secret_vars.iter().filter_map(|name| vars.get(name)) {
      collect_value_redactions(value, &mut redactions);
    }
    if request.raw && !redactions.is_empty() {
      return Err(ExecutorError::RawUnsupported(
        "raw execution with resolved secrets cannot be safely redacted".to_owned(),
      ));
    }

    let client = self
      .manager
      .get_client(registration.plugin_name())
      .await
      .map_err(|error| io::Error::new(io::ErrorKind::NotFound, error.to_string()))?;

    let command = match request.value {
      Value::String(command) => command,
      value => value.to_string(),
    };
    let execution = client
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
      .await;
    let mut execution = match execution {
      Ok(execution) => execution,
      Err(_) if cancel_token.is_cancelled() => {
        return Err(io::Error::new(io::ErrorKind::Interrupted, "Command cancelled").into());
      },
      Err(error) => return Err(io::Error::from(error).into()),
    };
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
      match self
        .terminal
        .connect(RawTerminalInput::new(execution.terminal_input()))
        .await
      {
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
      let mut failure_location = None;
      let mut artifacts = Vec::new();
      let mut reports = Vec::new();
      let mut output_redactor = OutputRedactor::new(&redactions);
      loop {
        match execution.receive_output(&cancel_token).await {
          Ok(Some(response)) => match route_plugin_response_redacted(
            response,
            &command_id,
            request.output.as_ref(),
            raw_session.as_mut(),
            &mut output,
            ResponseSecurity {
              plugin_name: &plugin_name,
              redactions: &redactions,
            },
            &mut output_redactor,
          )
          .await?
          {
            PluginResponseAction::Continue(location) => {
              failure_location = location.or(failure_location);
            },
            PluginResponseAction::RegisterArtifact(artifact) => artifacts.push(artifact),
            PluginResponseAction::RegisterReport(report) => reports.push(report),
            PluginResponseAction::Complete { code, outputs } => {
              terminal_response = true;
              flush_redacted_bytes(
                request.output.as_ref(),
                raw_session.as_mut(),
                &command_id,
                &mut output,
                &plugin_name,
                &mut output_redactor,
              )
              .await?;
              let outputs = if code == 0 {
                let outputs = Value::Object(outputs);
                registration.validate_outputs(&outputs).map_err(|error| {
                  ExecutorError::PluginOutputValidationFailed(request.target.name().to_owned(), error)
                })?;
                let Value::Object(outputs) = outputs else {
                  unreachable!("plugin outputs are constructed as an object")
                };
                outputs
              } else {
                outputs
              };
              let (stdout, stderr) = output.into_strings().await?;
              break Ok(PluginOutput {
                code,
                stdout,
                stderr,
                outputs,
                artifacts,
                reports,
                failure_location,
              });
            },
            PluginResponseAction::Error(message) => {
              terminal_response = true;
              break Err(io::Error::other(format!("Plugin error: {message}")).into());
            },
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

#[derive(Debug, Eq, PartialEq)]
enum PluginResponseAction {
  Continue(Option<SourceLocation>),
  RegisterArtifact(ArtifactDeclaration),
  RegisterReport(ReportDeclaration),
  Complete { code: i32, outputs: Map<String, Value> },
  Error(String),
}

async fn route_plugin_response_redacted(
  response: PluginResponse,
  command_id: &str,
  target: Option<&RuntimeOutput>,
  raw_session: Option<&mut octa_output::RawConsoleSession>,
  output: &mut OutputCapture,
  security: ResponseSecurity<'_>,
  output_redactor: &mut OutputRedactor,
) -> ExecutorResult<PluginResponseAction> {
  let ResponseSecurity {
    plugin_name,
    redactions,
  } = security;
  match response {
    PluginResponse::Stdout { id, line } if id == command_id => {
      if output_redactor.has_pending(ConsoleStream::Stdout) {
        let mut bytes = without_line_ending(&line).as_bytes().to_vec();
        bytes.push(b'\n');
        let mut bytes = output_redactor.push(ConsoleStream::Stdout, bytes);
        bytes.extend(output_redactor.finish(ConsoleStream::Stdout));
        output
          .append(ConsoleStream::Stdout, &bytes)
          .await
          .map_err(|error| capture_error(plugin_name, error))?;
        route_bytes(target, raw_session, command_id, ConsoleStream::Stdout, bytes).await?;
        return Ok(PluginResponseAction::Continue(None));
      }
      let line = redact(without_line_ending(&line), redactions);
      route_line(target, raw_session, command_id, ConsoleStream::Stdout, &line).await?;
      output
        .append_line(ConsoleStream::Stdout, &line)
        .await
        .map_err(|error| capture_error(plugin_name, error))?;
    },
    PluginResponse::Stderr { id, line } if id == command_id => {
      if output_redactor.has_pending(ConsoleStream::Stderr) {
        let mut bytes = without_line_ending(&line).as_bytes().to_vec();
        bytes.push(b'\n');
        let mut bytes = output_redactor.push(ConsoleStream::Stderr, bytes);
        bytes.extend(output_redactor.finish(ConsoleStream::Stderr));
        output
          .append(ConsoleStream::Stderr, &bytes)
          .await
          .map_err(|error| capture_error(plugin_name, error))?;
        route_bytes(target, raw_session, command_id, ConsoleStream::Stderr, bytes).await?;
        return Ok(PluginResponseAction::Continue(None));
      }
      let line = redact(without_line_ending(&line), redactions);
      route_line(target, raw_session, command_id, ConsoleStream::Stderr, &line).await?;
      output
        .append_line(ConsoleStream::Stderr, &line)
        .await
        .map_err(|error| capture_error(plugin_name, error))?;
    },
    PluginResponse::StdoutBytes { id, bytes } if id == command_id => {
      let bytes = output_redactor.push(ConsoleStream::Stdout, bytes);
      if bytes.is_empty() {
        return Ok(PluginResponseAction::Continue(None));
      }
      output
        .append(ConsoleStream::Stdout, &bytes)
        .await
        .map_err(|error| capture_error(plugin_name, error))?;
      route_bytes(target, raw_session, command_id, ConsoleStream::Stdout, bytes).await?;
    },
    PluginResponse::StderrBytes { id, bytes } if id == command_id => {
      let bytes = output_redactor.push(ConsoleStream::Stderr, bytes);
      if bytes.is_empty() {
        return Ok(PluginResponseAction::Continue(None));
      }
      output
        .append(ConsoleStream::Stderr, &bytes)
        .await
        .map_err(|error| capture_error(plugin_name, error))?;
      route_bytes(target, raw_session, command_id, ConsoleStream::Stderr, bytes).await?;
    },
    PluginResponse::Progress { id, progress } if id == command_id => {
      // A raw session owns the console render lock until it ends; progress is presentation-only
      // and cannot be interleaved with its terminal byte stream.
      if raw_session.is_none() {
        if let Some(target) = target {
          target
            .progress(
              command_id,
              ProgressUpdate {
                message: redact(&progress.message, redactions),
                current: progress.current,
                total: progress.total,
                unit: progress.unit,
              },
            )
            .await?;
        }
      }
    },
    PluginResponse::RegisterArtifact { id, artifact } if id == command_id => {
      return Ok(PluginResponseAction::RegisterArtifact(artifact));
    },
    PluginResponse::RegisterReport { id, report } if id == command_id => {
      return Ok(PluginResponseAction::RegisterReport(report));
    },
    PluginResponse::Diagnostic {
      id,
      level,
      message,
      location,
    } if id == command_id => {
      let is_error = matches!(level, PluginDiagnosticLevel::Error);
      let level = match level {
        PluginDiagnosticLevel::Trace => octa_output::ConsoleLevel::Trace,
        PluginDiagnosticLevel::Debug => octa_output::ConsoleLevel::Debug,
        PluginDiagnosticLevel::Info => octa_output::ConsoleLevel::Info,
        PluginDiagnosticLevel::Warn => octa_output::ConsoleLevel::Warn,
        PluginDiagnosticLevel::Error => octa_output::ConsoleLevel::Error,
      };
      let location = location.map(|location| SourceLocation {
        file: location.file,
        line: location.line,
        column: location.column,
      });
      if let Some(target) = target {
        target
          .diagnostic(level, redact(&message, redactions), location.clone())
          .await?;
      }
      return Ok(PluginResponseAction::Continue(is_error.then_some(location).flatten()));
    },
    PluginResponse::Completed { id, code, outputs } if id == command_id => {
      return Ok(PluginResponseAction::Complete { code, outputs });
    },
    PluginResponse::Error { id, message } if id == command_id => {
      return Ok(PluginResponseAction::Error(redact(&message, redactions)));
    },
    _ => {},
  }
  Ok(PluginResponseAction::Continue(None))
}

async fn route_line(
  target: Option<&RuntimeOutput>,
  raw_session: Option<&mut octa_output::RawConsoleSession>,
  command_id: &str,
  stream: ConsoleStream,
  line: &str,
) -> io::Result<()> {
  if let Some(session) = raw_session {
    if !target.is_some_and(|target| target.hides(stream)) {
      let mut bytes = line.as_bytes().to_vec();
      bytes.push(b'\n');
      session.write(stream, bytes).await?;
    }
  } else if let Some(target) = target {
    target.line(command_id, stream, line.to_owned()).await?;
  }
  Ok(())
}

async fn route_bytes(
  target: Option<&RuntimeOutput>,
  raw_session: Option<&mut octa_output::RawConsoleSession>,
  command_id: &str,
  stream: ConsoleStream,
  bytes: Vec<u8>,
) -> io::Result<()> {
  if let Some(session) = raw_session {
    if !target.is_some_and(|target| target.hides(stream)) {
      session.write(stream, bytes).await?;
    }
  } else if let Some(target) = target {
    target.bytes(command_id, stream, bytes).await?;
  }
  Ok(())
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

struct OutputRedactor {
  stdout: Vec<u8>,
  stderr: Vec<u8>,
  needles: Vec<Vec<u8>>,
  reserve: usize,
}

#[derive(Clone, Copy)]
struct ResponseSecurity<'a> {
  plugin_name: &'a str,
  redactions: &'a [String],
}

impl OutputRedactor {
  fn new(redactions: &[String]) -> Self {
    let needles = redactions
      .iter()
      .filter(|value| !value.is_empty())
      .map(|value| value.as_bytes().to_vec())
      .collect::<Vec<_>>();
    let reserve = needles.iter().map(Vec::len).max().unwrap_or(1).saturating_sub(1);
    Self {
      stdout: Vec::new(),
      stderr: Vec::new(),
      needles,
      reserve,
    }
  }

  fn push(&mut self, stream: ConsoleStream, bytes: Vec<u8>) -> Vec<u8> {
    let buffer = match stream {
      ConsoleStream::Stdout => &mut self.stdout,
      ConsoleStream::Stderr => &mut self.stderr,
    };
    buffer.extend(bytes);
    emit_redacted_prefix(buffer, &self.needles, self.reserve)
  }

  fn finish(&mut self, stream: ConsoleStream) -> Vec<u8> {
    let buffer = match stream {
      ConsoleStream::Stdout => &mut self.stdout,
      ConsoleStream::Stderr => &mut self.stderr,
    };
    redact_bytes(std::mem::take(buffer), &self.needles)
  }

  fn has_pending(&self, stream: ConsoleStream) -> bool {
    match stream {
      ConsoleStream::Stdout => !self.stdout.is_empty(),
      ConsoleStream::Stderr => !self.stderr.is_empty(),
    }
  }
}

fn emit_redacted_prefix(buffer: &mut Vec<u8>, needles: &[Vec<u8>], reserve: usize) -> Vec<u8> {
  if needles.is_empty() {
    return std::mem::take(buffer);
  }
  let mut cut = buffer.len().saturating_sub(reserve);
  loop {
    let previous = cut;
    for needle in needles {
      for start in find_all(buffer, needle) {
        let end = start + needle.len();
        if start < cut && end > cut {
          cut = end;
        }
      }
    }
    if cut == previous {
      break;
    }
  }
  let suffix = buffer.split_off(cut);
  let prefix = std::mem::replace(buffer, suffix);
  redact_bytes(prefix, needles)
}

fn redact_bytes(mut bytes: Vec<u8>, needles: &[Vec<u8>]) -> Vec<u8> {
  for needle in needles {
    let mut start = 0;
    while let Some(relative) = bytes[start..].windows(needle.len()).position(|window| window == needle) {
      let found = start + relative;
      bytes.splice(found..found + needle.len(), b"*****".iter().copied());
      start = found + 5;
    }
  }
  bytes
}

fn find_all(bytes: &[u8], needle: &[u8]) -> Vec<usize> {
  bytes
    .windows(needle.len())
    .enumerate()
    .filter_map(|(index, candidate)| (candidate == needle).then_some(index))
    .collect()
}

async fn flush_redacted_bytes(
  target: Option<&RuntimeOutput>,
  mut raw_session: Option<&mut octa_output::RawConsoleSession>,
  command_id: &str,
  output: &mut OutputCapture,
  plugin_name: &str,
  redactor: &mut OutputRedactor,
) -> ExecutorResult<()> {
  for stream in [ConsoleStream::Stdout, ConsoleStream::Stderr] {
    let bytes = redactor.finish(stream);
    if bytes.is_empty() {
      continue;
    }
    output
      .append(stream, &bytes)
      .await
      .map_err(|error| capture_error(plugin_name, error))?;
    route_bytes(target, raw_session.as_deref_mut(), command_id, stream, bytes).await?;
  }
  Ok(())
}

#[cfg(test)]
async fn route_plugin_response(
  response: PluginResponse,
  command_id: &str,
  target: Option<&RuntimeOutput>,
  raw_session: Option<&mut octa_output::RawConsoleSession>,
  output: &mut OutputCapture,
  plugin_name: &str,
) -> ExecutorResult<PluginResponseAction> {
  let mut redactor = OutputRedactor::new(&[]);
  route_plugin_response_redacted(
    response,
    command_id,
    target,
    raw_session,
    output,
    ResponseSecurity {
      plugin_name,
      redactions: &[],
    },
    &mut redactor,
  )
  .await
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
  uses: Option<Arc<StdMutex<HashSet<PluginTarget>>>>,
}

impl ManagerPluginEvaluator {
  pub(crate) fn new(manager: Arc<PluginManager>) -> Self {
    Self {
      invoker: PluginInvoker::new(manager),
      uses: None,
    }
  }

  /// Records the selectors actually evaluated while resolving task values,
  /// conditions, and preconditions for later action-key construction.
  pub(crate) fn tracking(manager: Arc<PluginManager>, uses: Arc<StdMutex<HashSet<PluginTarget>>>) -> Self {
    Self {
      invoker: PluginInvoker::new(manager),
      uses: Some(uses),
    }
  }
}

#[async_trait]
impl PluginEvaluator for ManagerPluginEvaluator {
  async fn evaluate(&self, request: EvaluationRequest, cancel_token: CancellationToken) -> ExecutorResult<String> {
    let key = request.target.name().to_owned();
    if let Some(uses) = &self.uses {
      uses
        .lock()
        .map_err(|_| ExecutorError::LockError("plugin use tracker poisoned".to_owned()))?
        .insert(request.target.clone());
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
        location: output.failure_location,
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
      location: None,
    })
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};

  use octa_octafile::Silence;
  use octa_output::{
    Console, ConsoleEntry, ConsolePayload, ConsoleRecord, ConsoleRenderer, ConsoleScopeAllocator, ExecutionEvent,
  };
  use octa_plugin::protocol::{
    ArtifactDeclaration as PluginArtifact, ProgressUpdate as PluginProgressUpdate, ReportDeclaration as PluginReport,
    SourceLocation as PluginSourceLocation,
  };

  use super::*;

  #[test]
  fn redacts_secrets_split_across_binary_frames() {
    let mut redactor = OutputRedactor::new(&["split-secret".to_owned()]);
    let mut output = redactor.push(ConsoleStream::Stdout, b"before split-".to_vec());
    output.extend(redactor.push(ConsoleStream::Stdout, b"secret after".to_vec()));
    output.extend(redactor.finish(ConsoleStream::Stdout));

    let output = String::from_utf8(output).unwrap();
    assert_eq!(output, "before ***** after");
    assert!(!output.contains("split-secret"));
  }

  #[test]
  fn redacts_secrets_across_binary_and_line_frames() {
    let mut redactor = OutputRedactor::new(&["split-secret".to_owned()]);
    let mut output = redactor.push(ConsoleStream::Stdout, b"before split-".to_vec());
    output.extend(redactor.push(ConsoleStream::Stdout, b"secret\n".to_vec()));
    output.extend(redactor.finish(ConsoleStream::Stdout));

    let output = String::from_utf8(output).unwrap();
    assert_eq!(output, "before *****\n");
    assert!(!output.contains("split-secret"));
  }

  #[derive(Clone, Default)]
  struct Recording(Arc<Mutex<Vec<ConsoleRecord>>>);

  impl ConsoleRenderer for Recording {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.lock().unwrap().push(entry.record().clone());
      Ok(())
    }
  }

  #[test]
  fn removes_protocol_line_endings_without_trimming_output() {
    assert_eq!(without_line_ending("  value  \r\n"), "  value  ");
    assert_eq!(without_line_ending("  value  "), "  value  ");
    assert_eq!(without_line_ending("\n"), "");
  }

  #[tokio::test]
  async fn routes_every_structured_plugin_response_and_captures_output() {
    let records = Recording::default();
    let console = Arc::new(Console::new(records.clone()));
    let allocator = ConsoleScopeAllocator::default();
    let scope = allocator.scope("task");
    let step = allocator.step(&scope, "shell");
    let step_id = step.id();
    let target = RuntimeOutput::with_silence(
      console.clone(),
      7,
      Some(crate::task::ExecutionBinding::for_step(scope, step)),
      Silence::None,
    );
    let mut output = OutputCapture::default();

    let responses = [
      PluginResponse::Stdout {
        id: "command".to_owned(),
        line: "out\r\n".to_owned(),
      },
      PluginResponse::Stderr {
        id: "command".to_owned(),
        line: "err\n".to_owned(),
      },
      PluginResponse::StdoutBytes {
        id: "command".to_owned(),
        bytes: b"-bytes".to_vec(),
      },
      PluginResponse::StderrBytes {
        id: "command".to_owned(),
        bytes: b"-bytes".to_vec(),
      },
    ];
    for response in responses {
      assert_eq!(
        route_plugin_response(response, "command", Some(&target), None, &mut output, "shell")
          .await
          .unwrap(),
        PluginResponseAction::Continue(None)
      );
    }

    for level in [
      PluginDiagnosticLevel::Trace,
      PluginDiagnosticLevel::Debug,
      PluginDiagnosticLevel::Info,
      PluginDiagnosticLevel::Warn,
      PluginDiagnosticLevel::Error,
    ] {
      let is_error = matches!(level, PluginDiagnosticLevel::Error);
      let action = route_plugin_response(
        PluginResponse::Diagnostic {
          id: "command".to_owned(),
          level,
          message: "diagnostic".to_owned(),
          location: Some(PluginSourceLocation {
            file: "Octafile.yml".to_owned(),
            line: Some(3),
            column: Some(5),
          }),
        },
        "command",
        Some(&target),
        None,
        &mut output,
        "shell",
      )
      .await
      .unwrap();
      assert_eq!(
        action,
        PluginResponseAction::Continue(is_error.then_some(SourceLocation {
          file: "Octafile.yml".to_owned(),
          line: Some(3),
          column: Some(5),
        }))
      );
    }

    assert_eq!(
      route_plugin_response(
        PluginResponse::Progress {
          id: "command".to_owned(),
          progress: PluginProgressUpdate {
            message: "Compiling".to_owned(),
            current: Some(3),
            total: Some(10),
            unit: Some("files".to_owned()),
          },
        },
        "command",
        Some(&target),
        None,
        &mut output,
        "shell",
      )
      .await
      .unwrap(),
      PluginResponseAction::Continue(None)
    );
    let artifact = PluginArtifact {
      name: "binary".to_owned(),
      path: "dist/app".into(),
      content_type: None,
    };
    assert_eq!(
      route_plugin_response(
        PluginResponse::RegisterArtifact {
          id: "command".to_owned(),
          artifact: artifact.clone(),
        },
        "command",
        Some(&target),
        None,
        &mut output,
        "shell",
      )
      .await
      .unwrap(),
      PluginResponseAction::RegisterArtifact(artifact)
    );
    let report = PluginReport {
      name: "tests".to_owned(),
      path: "junit.xml".into(),
      format: "junit".to_owned(),
    };
    assert_eq!(
      route_plugin_response(
        PluginResponse::RegisterReport {
          id: "command".to_owned(),
          report: report.clone(),
        },
        "command",
        Some(&target),
        None,
        &mut output,
        "shell",
      )
      .await
      .unwrap(),
      PluginResponseAction::RegisterReport(report)
    );

    assert_eq!(
      route_plugin_response(
        PluginResponse::Completed {
          id: "command".to_owned(),
          code: 4,
          outputs: serde_json::Map::from_iter([("digest".to_owned(), serde_json::json!("sha256:test"))]),
        },
        "command",
        Some(&target),
        None,
        &mut output,
        "shell",
      )
      .await
      .unwrap(),
      PluginResponseAction::Complete {
        code: 4,
        outputs: serde_json::Map::from_iter([("digest".to_owned(), serde_json::json!("sha256:test"))]),
      }
    );
    assert_eq!(
      route_plugin_response(
        PluginResponse::Error {
          id: "command".to_owned(),
          message: "failed".to_owned(),
        },
        "command",
        Some(&target),
        None,
        &mut output,
        "shell",
      )
      .await
      .unwrap(),
      PluginResponseAction::Error("failed".to_owned())
    );
    assert_eq!(
      route_plugin_response(
        PluginResponse::Stdout {
          id: "another-command".to_owned(),
          line: "ignored".to_owned(),
        },
        "command",
        None,
        None,
        &mut output,
        "shell",
      )
      .await
      .unwrap(),
      PluginResponseAction::Continue(None)
    );

    let (stdout, stderr) = output.into_strings().await.unwrap();
    assert_eq!(stdout, "out\n-bytes");
    assert_eq!(stderr, "err\n-bytes");
    console.drain().await.unwrap();
    let records = records.0.lock().unwrap();
    assert_eq!(
      records
        .iter()
        .filter(|record| matches!(record, ConsoleRecord::Execution(ExecutionEvent::Output { .. })))
        .count(),
      4
    );
    assert_eq!(
      records
        .iter()
        .filter(|record| matches!(record, ConsoleRecord::Diagnostic(_)))
        .count(),
      5
    );
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Execution(ExecutionEvent::Progress {
        step_id: Some(actual),
        progress: ProgressUpdate {
          current: Some(3),
          total: Some(10),
          ..
        },
        ..
      }) if *actual == step_id
    )));
    assert!(records.iter().all(|record| match record {
      ConsoleRecord::Execution(ExecutionEvent::Output { step_id: actual, .. }) => *actual == Some(step_id),
      ConsoleRecord::Diagnostic(diagnostic) => diagnostic.step_id == Some(step_id),
      _ => true,
    }));
  }

  #[tokio::test]
  async fn redacts_secrets_split_between_byte_and_line_responses() {
    let mut output = OutputCapture::default();
    let mut redactor = OutputRedactor::new(&["split-secret".to_owned()]);
    for (stream, bytes, line) in [
      (ConsoleStream::Stdout, b"out split-".to_vec(), "secret\n"),
      (ConsoleStream::Stderr, b"err split-".to_vec(), "secret\n"),
    ] {
      let bytes_response = match stream {
        ConsoleStream::Stdout => PluginResponse::StdoutBytes {
          id: "command".to_owned(),
          bytes,
        },
        ConsoleStream::Stderr => PluginResponse::StderrBytes {
          id: "command".to_owned(),
          bytes,
        },
      };
      route_plugin_response_redacted(
        bytes_response,
        "command",
        None,
        None,
        &mut output,
        ResponseSecurity {
          plugin_name: "shell",
          redactions: &["split-secret".to_owned()],
        },
        &mut redactor,
      )
      .await
      .unwrap();
      let line_response = match stream {
        ConsoleStream::Stdout => PluginResponse::Stdout {
          id: "command".to_owned(),
          line: line.to_owned(),
        },
        ConsoleStream::Stderr => PluginResponse::Stderr {
          id: "command".to_owned(),
          line: line.to_owned(),
        },
      };
      route_plugin_response_redacted(
        line_response,
        "command",
        None,
        None,
        &mut output,
        ResponseSecurity {
          plugin_name: "shell",
          redactions: &["split-secret".to_owned()],
        },
        &mut redactor,
      )
      .await
      .unwrap();
    }
    let (stdout, stderr) = output.into_strings().await.unwrap();
    assert_eq!(stdout, "out *****\n");
    assert_eq!(stderr, "err *****\n");
  }

  #[tokio::test]
  async fn raw_routing_preserves_bytes_and_honors_stream_silence() {
    let records = Recording::default();
    let console = Arc::new(Console::new(records.clone()));
    let allocator = ConsoleScopeAllocator::default();
    let scope = allocator.scope("raw");
    let step = allocator.step(&scope, "shell");
    let step_id = step.id();
    let target = RuntimeOutput::with_silence(
      console.clone(),
      1,
      Some(crate::task::ExecutionBinding::for_step(scope, step)),
      Silence::Stdout,
    );
    let mut session = target.begin_raw("command").await.unwrap().unwrap();
    let mut output = OutputCapture::default();

    for response in [
      PluginResponse::Stdout {
        id: "command".to_owned(),
        line: "hidden".to_owned(),
      },
      PluginResponse::Stderr {
        id: "command".to_owned(),
        line: "line".to_owned(),
      },
      PluginResponse::StdoutBytes {
        id: "command".to_owned(),
        bytes: b"hidden".to_vec(),
      },
      PluginResponse::StderrBytes {
        id: "command".to_owned(),
        bytes: b"bytes".to_vec(),
      },
      PluginResponse::Progress {
        id: "command".to_owned(),
        progress: PluginProgressUpdate {
          message: "ignored during raw execution".to_owned(),
          current: None,
          total: None,
          unit: None,
        },
      },
    ] {
      route_plugin_response(
        response,
        "command",
        Some(&target),
        Some(&mut session),
        &mut output,
        "shell",
      )
      .await
      .unwrap();
    }
    drop(session);
    console.drain().await.unwrap();

    let payloads = records
      .0
      .lock()
      .unwrap()
      .iter()
      .filter_map(|record| match record {
        ConsoleRecord::Execution(ExecutionEvent::Output { payload, .. }) => Some(payload.clone()),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(
      payloads,
      [
        ConsolePayload::RawBytes(b"line\n".to_vec()),
        ConsolePayload::RawBytes(b"bytes".to_vec())
      ]
    );
    assert!(records.0.lock().unwrap().iter().all(|record| match record {
      ConsoleRecord::Execution(ExecutionEvent::Output { step_id: actual, .. }) => *actual == Some(step_id),
      ConsoleRecord::Execution(ExecutionEvent::Progress { .. }) => false,
      _ => true,
    }));
  }

  #[test]
  fn capture_errors_retain_their_specific_failure() {
    assert!(matches!(
      capture_error("shell", CaptureError::LimitExceeded),
      ExecutorError::PluginOutputTooLarge { plugin, .. } if plugin == "shell"
    ));
    assert!(matches!(
      capture_error("shell", CaptureError::Io(io::Error::other("disk"))),
      ExecutorError::IoError(_)
    ));
  }
}
