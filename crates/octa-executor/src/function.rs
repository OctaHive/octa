use std::{
  collections::HashMap,
  error::Error,
  path::{Path, PathBuf},
  process::{Command, Stdio},
};

#[cfg(windows)]
#[allow(unused_imports)]
use std::os::windows::process::CommandExt;
use tera::{Filter, Function, Result, Value};
use tracing::{debug, info};

use octa_plugin::logger::redact;

/// Tera function and filter that evaluate a command in the context of their value layer.
#[derive(Clone)]
pub struct ExecuteShell {
  current_dir: PathBuf,
  environment: HashMap<String, String>,
  // Resolved secrets inherited by the command and therefore safe to replace literally.
  redactions: Vec<String>,
  // A secret `sh` value hides the whole producer command, not just inherited values within it.
  redact_commands: bool,
  dry: bool,
}

impl ExecuteShell {
  pub fn new(current_dir: impl Into<PathBuf>, environment: HashMap<String, String>, dry: bool) -> Self {
    Self {
      current_dir: current_dir.into(),
      environment,
      redactions: Vec::new(),
      redact_commands: false,
      dry,
    }
  }

  /// Configures diagnostic redaction without changing the command or its environment.
  fn with_redactions(mut self, redactions: Vec<String>, redact_commands: bool) -> Self {
    self.redactions = redactions;
    self.redact_commands = redact_commands;
    self
  }

  pub(crate) fn execute(&self, command_text: &str) -> Result<String> {
    // Execution receives the original command; only diagnostic text uses this sanitized copy.
    let logged_command = if self.redact_commands {
      "*****".to_owned()
    } else {
      redact(command_text, &self.redactions)
    };
    if self.dry {
      info!(
        "Execute command in directory {}: {}",
        self.current_dir.display(),
        logged_command
      );
      return Ok(String::new());
    }

    debug!(
      "Execute command in directory {}: {}",
      self.current_dir.display(),
      logged_command
    );

    #[cfg(windows)]
    let mut command = {
      const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
      const CREATE_NO_WINDOW: u32 = 0x08000000;

      let mut cmd = Command::new("cmd");
      cmd
        .current_dir(&self.current_dir)
        .args(["/C", command_text])
        .envs(&self.environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
      cmd
    };

    #[cfg(not(windows))]
    let mut command = {
      let mut cmd = Command::new("sh");
      cmd
        .current_dir(&self.current_dir)
        .arg("-c")
        .arg(command_text)
        .envs(&self.environment)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
      cmd
    };

    let output = command
      .output()
      .map_err(|error| tera::Error::msg(format!("Failed to execute command '{logged_command}': {error}")))?;

    if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
      // A failed secret producer may print the newly obtained value before Octa can collect it.
      let logged_stderr = if self.redact_commands {
        "*****".to_owned()
      } else {
        redact(&stderr, &self.redactions)
      };
      let status = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
      return Err(tera::Error::msg(format!(
        "Shell command '{logged_command}' failed with status {status}: {}",
        logged_stderr
      )));
    }

    Ok(String::from_utf8_lossy(output.stdout.trim_ascii_end()).to_string())
  }
}

impl Function for ExecuteShell {
  fn call(&self, args: &HashMap<String, Value>) -> Result<Value> {
    let sh = args
      .get("command")
      .ok_or_else(|| tera::Error::msg("Missing 'command' argument"))?;
    let command_text = sh.as_str().ok_or_else(|| tera::Error::msg("Wrong command format"))?;

    self.execute(command_text).map(Value::String)
  }
}

impl Filter for ExecuteShell {
  fn filter(&self, value: &Value, args: &HashMap<String, Value>) -> Result<Value> {
    if !args.is_empty() {
      return Err(tera::Error::msg("The 'shell' filter does not accept arguments"));
    }

    let command_text = value
      .as_str()
      .ok_or_else(|| tera::Error::msg("The 'shell' filter expects a string"))?;
    self.execute(command_text).map(Value::String)
  }
}

/// Registers both shell template forms and returns the same runner for structured `sh` values.
#[cfg(test)]
fn register_shell(
  tera: &mut tera::Tera,
  current_dir: &Path,
  environment: HashMap<String, String>,
  dry: bool,
) -> ExecuteShell {
  register_shell_with_redactions(tera, current_dir, environment, dry, Vec::new(), false)
}

/// Registers shell template helpers while redacting inherited secret values from diagnostics.
pub(crate) fn register_shell_with_redactions(
  tera: &mut tera::Tera,
  current_dir: &Path,
  environment: HashMap<String, String>,
  dry: bool,
  redactions: Vec<String>,
  redact_commands: bool,
) -> ExecuteShell {
  let shell =
    ExecuteShell::new(current_dir.to_path_buf(), environment, dry).with_redactions(redactions, redact_commands);
  tera.register_function("shell", shell.clone());
  tera.register_filter("shell", shell.clone());
  shell
}

/// Includes nested function errors that Tera omits from its top-level display message.
pub fn format_tera_error(error: &tera::Error) -> String {
  let mut messages = vec![error.to_string()];
  let mut source = error.source();

  while let Some(error) = source {
    messages.push(error.to_string());
    source = error.source();
  }

  messages.join(": ")
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::TempDir;
  use tera::Value;

  fn setup_test_command() -> (HashMap<String, Value>, TempDir) {
    let temp_dir = TempDir::new().unwrap();
    let mut args = HashMap::new();
    args.insert("command".to_string(), Value::String("echo 'test'".to_string()));
    (args, temp_dir)
  }

  #[test]
  fn test_execute_shell_echo() {
    let (args, temp_dir) = setup_test_command();
    let execute_shell = ExecuteShell::new(temp_dir.path(), HashMap::new(), false);

    let result = execute_shell.call(&args).unwrap();

    match result {
      Value::String(output) => {
        #[cfg(windows)]
        assert_eq!(output.trim(), "'test'");
        #[cfg(not(windows))]
        assert_eq!(output.trim(), "test");
      },
      _ => panic!("Expected string output"),
    }
  }

  #[test]
  fn test_execute_shell_filter() {
    let temp_dir = TempDir::new().unwrap();
    let mut tera = tera::Tera::default();
    register_shell(&mut tera, temp_dir.path(), HashMap::new(), false);

    let result = tera
      .render_str(r#"{{ "echo filtered" | shell }}"#, &tera::Context::new())
      .unwrap();

    assert_eq!(result.trim(), "filtered");
  }

  #[test]
  fn test_execute_shell_missing_command() {
    let temp_dir = TempDir::new().unwrap();
    let execute_shell = ExecuteShell::new(temp_dir.path(), HashMap::new(), false);
    let args = HashMap::new();

    let result = execute_shell.call(&args);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().to_string(), "Missing 'command' argument");
  }

  #[test]
  fn test_execute_shell_invalid_command() {
    let mut args = HashMap::new();
    args.insert("command".to_string(), Value::Bool(true));
    let temp_dir = TempDir::new().unwrap();
    let execute_shell = ExecuteShell::new(temp_dir.path(), HashMap::new(), false);

    let result = execute_shell.call(&args);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().to_string(), "Wrong command format");
  }

  #[test]
  fn test_execute_shell_file_operations() {
    let temp_dir = TempDir::new().unwrap();
    let test_file_path = temp_dir.path().join("test.txt");

    // Create a command to write to a file
    let mut args = HashMap::new();
    #[cfg(windows)]
    let command = format!("echo hello > {}", test_file_path.display());
    #[cfg(not(windows))]
    let command = format!("echo hello > {}", test_file_path.display());

    args.insert("command".to_string(), Value::String(command));

    let execute_shell = ExecuteShell::new(temp_dir.path(), HashMap::new(), false);
    let _ = execute_shell.call(&args).unwrap();

    assert!(test_file_path.exists());
    let content = fs::read_to_string(test_file_path).unwrap();
    assert!(content.contains("hello"));
  }

  #[test]
  fn test_execute_shell_dry() {
    let (args, temp_dir) = setup_test_command();
    let execute_shell_dry = ExecuteShell::new(temp_dir.path(), HashMap::new(), true);

    let result = execute_shell_dry.call(&args).unwrap();
    assert_eq!(result, Value::String(String::new()));
  }

  #[test]
  fn test_execute_shell_dry_missing_command() {
    let temp_dir = TempDir::new().unwrap();
    let execute_shell_dry = ExecuteShell::new(temp_dir.path(), HashMap::new(), true);
    let args = HashMap::new();

    let result = execute_shell_dry.call(&args);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().to_string(), "Missing 'command' argument");
  }

  #[test]
  fn test_execute_shell_dry_invalid_command() {
    let mut args = HashMap::new();
    args.insert("command".to_string(), Value::Bool(true));
    let temp_dir = TempDir::new().unwrap();
    let execute_shell_dry = ExecuteShell::new(temp_dir.path(), HashMap::new(), true);

    let result = execute_shell_dry.call(&args);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().to_string(), "Wrong command format");
  }
}
