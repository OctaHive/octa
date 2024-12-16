use std::{
  collections::HashMap,
  env,
  process::{Command, Stdio},
};

#[cfg(windows)]
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
        .args(["/C", &command])
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
