use std::{collections::HashMap, env, process::Command};

use tera::{Function, Result, Value};
use tracing::debug;

pub struct ExecuteShell;

impl Function for ExecuteShell {
  fn call(&self, args: &HashMap<String, Value>) -> Result<Value> {
    let sh = args
      .get("command")
      .ok_or_else(|| tera::Error::msg("Missing 'command' argument"))?;
    let os_type = sys_info::os_type().map(|os| os.to_lowercase()).unwrap_or_default();
    let current_dir = env::current_dir()?;
    let command = sh.as_str().unwrap();

    debug!("Execute command in directory {}: {}", current_dir.display(), command);

    let output = match os_type.as_str() {
      "windows" => Command::new("cmd")
        .args(["/C", command])
        .current_dir(current_dir)
        .output()
        .map_err(|e| tera::Error::msg(format!("Failed to execute command {} for arg: {}", sh, e)))?,
      _ => Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(current_dir)
        .output()
        .map_err(|e| tera::Error::msg(format!("Failed to execute command {} for arg: {}", sh, e)))?,
    };

    let res = Value::String(String::from_utf8_lossy(&output.stdout.trim_ascii()).to_string());

    debug!("Command output result: {:?}", res);

    Ok(res)
  }
}
