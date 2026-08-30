use std::{
  collections::HashMap,
  error::Error,
  path::{Path, PathBuf},
  process::{Command, Stdio},
};

#[cfg(windows)]
#[allow(unused_imports)]
use std::os::windows::process::CommandExt;
use tera::{Function, Result, Value};
use tracing::{debug, info};

/// Tera function that evaluates a command in the directory and environment of its value layer.
pub struct ExecuteShell {
  current_dir: PathBuf,
  environment: HashMap<String, String>,
  dry: bool,
}

impl ExecuteShell {
  pub fn new(current_dir: impl Into<PathBuf>, environment: HashMap<String, String>, dry: bool) -> Self {
    Self {
      current_dir: current_dir.into(),
      environment,
      dry,
    }
  }
}

impl Function for ExecuteShell {
  fn call(&self, args: &HashMap<String, Value>) -> Result<Value> {
    let sh = args
      .get("command")
      .ok_or_else(|| tera::Error::msg("Missing 'command' argument"))?;
    let command_text = sh.as_str().ok_or_else(|| tera::Error::msg("Wrong command format"))?;

    if self.dry {
      info!(
        "Execute command in directory {}: {}",
        self.current_dir.display(),
        command_text
      );
      return Ok(Value::Null);
    }

    debug!(
      "Execute command in directory {}: {}",
      self.current_dir.display(),
      command_text
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
      .map_err(|e| tera::Error::msg(format!("Failed to execute command {} for arg: {}", sh, e)))?;

    if !output.status.success() {
      let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
      let status = output
        .status
        .code()
        .map_or_else(|| "signal".to_string(), |code| code.to_string());
      return Err(tera::Error::msg(format!(
        "Shell command '{command_text}' failed with status {status}: {stderr}"
      )));
    }

    let res = Value::String(String::from_utf8_lossy(output.stdout.trim_ascii_end()).to_string());

    debug!("Command output result: {:?}", res);

    Ok(res)
  }
}

/// Registers the shell function with the execution context for the current vars or env layer.
pub fn register_shell_function(
  tera: &mut tera::Tera,
  current_dir: &Path,
  environment: HashMap<String, String>,
  dry: bool,
) {
  tera.register_function("shell", ExecuteShell::new(current_dir.to_path_buf(), environment, dry));
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
    assert!(matches!(result, Value::Null));
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
