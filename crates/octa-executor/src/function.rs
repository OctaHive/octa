use std::{
  collections::HashMap,
  env,
  process::{Command, Stdio},
};

#[cfg(windows)]
#[allow(unused_imports)]
use std::os::windows::process::CommandExt;
use tera::{Function, Result, Value};
use tracing::{debug, info};

pub struct ExecuteShell;

impl Function for ExecuteShell {
  fn call(&self, args: &HashMap<String, Value>) -> Result<Value> {
    let sh = args
      .get("command")
      .ok_or_else(|| tera::Error::msg("Missing 'command' argument"))?;
    let current_dir = env::current_dir().map_err(|_| tera::Error::msg("Can;t get current directory"))?;
    let command = sh.as_str().ok_or_else(|| tera::Error::msg("Wrong command format"))?;

    debug!("Execute command in directory {}: {}", current_dir.display(), command);

    #[cfg(windows)]
    let mut command = {
      const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
      const CREATE_NO_WINDOW: u32 = 0x08000000;

      let mut cmd = Command::new("cmd");
      cmd
        .current_dir(current_dir)
        .args(["/C", command])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
      cmd
    };

    #[cfg(not(windows))]
    let mut command = {
      let mut cmd = Command::new("sh");
      cmd
        .current_dir(&current_dir)
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
      cmd
    };

    let output = command
      .output()
      .map_err(|e| tera::Error::msg(format!("Failed to execute command {} for arg: {}", sh, e)))?;

    let res = Value::String(String::from_utf8_lossy(output.stdout.trim_ascii()).to_string());

    debug!("Command output result: {:?}", res);

    Ok(res)
  }
}

pub struct ExecuteShellDry;

impl Function for ExecuteShellDry {
  fn call(&self, args: &HashMap<String, Value>) -> Result<Value> {
    let sh = args
      .get("command")
      .ok_or_else(|| tera::Error::msg("Missing 'command' argument"))?;
    let current_dir = env::current_dir().map_err(|_| tera::Error::msg("Can;t get current directory"))?;
    let command = sh.as_str().ok_or_else(|| tera::Error::msg("Wrong command format"))?;

    info!("Execute command in directory {}: {}", current_dir.display(), command);

    Ok(Value::Null)
  }
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
    let (args, _temp_dir) = setup_test_command();
    let execute_shell = ExecuteShell;

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
    let execute_shell = ExecuteShell;
    let args = HashMap::new();

    let result = execute_shell.call(&args);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().to_string(), "Missing 'command' argument");
  }

  #[test]
  fn test_execute_shell_invalid_command() {
    let mut args = HashMap::new();
    args.insert("command".to_string(), Value::Bool(true));
    let execute_shell = ExecuteShell;

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

    let execute_shell = ExecuteShell;
    let _ = execute_shell.call(&args).unwrap();

    assert!(test_file_path.exists());
    let content = fs::read_to_string(test_file_path).unwrap();
    assert!(content.contains("hello"));
  }

  #[test]
  fn test_execute_shell_dry() {
    let (args, _temp_dir) = setup_test_command();
    let execute_shell_dry = ExecuteShellDry;

    let result = execute_shell_dry.call(&args).unwrap();
    assert!(matches!(result, Value::Null));
  }

  #[test]
  fn test_execute_shell_dry_missing_command() {
    let execute_shell_dry = ExecuteShellDry;
    let args = HashMap::new();

    let result = execute_shell_dry.call(&args);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().to_string(), "Missing 'command' argument");
  }

  #[test]
  fn test_execute_shell_dry_invalid_command() {
    let mut args = HashMap::new();
    args.insert("command".to_string(), Value::Bool(true));
    let execute_shell_dry = ExecuteShellDry;

    let result = execute_shell_dry.call(&args);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().to_string(), "Wrong command format");
  }
}
