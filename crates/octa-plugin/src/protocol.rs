use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Version {
  pub version: String,
  pub features: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Schema {
  pub key: String,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub capabilities: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub validation_schema: Option<Map<String, Value>>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "payload")]
pub enum OctaCommand {
  Hello(Version),
  Schema,
  Execute {
    params: String,
    args: Vec<String>,
    dir: PathBuf,
    envs: HashMap<String, String>,
    vars: HashMap<String, Value>,
    /// Variable names whose resolved values the plugin SDK must redact from diagnostics.
    /// The default keeps the wire protocol compatible with runners that predate secret variables.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    secret_vars: Vec<String>,
    /// Hides the complete plugin payload from diagnostics for secret-producing evaluations.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    redact_params: bool,
    dry: bool,
  },
  Cancel {
    id: String,
  },
  Shutdown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "payload")]
pub enum PluginResponse {
  Hello(Version),
  Schema(Schema),
  Started { id: String },
  Stdout { id: String, line: String },
  Stderr { id: String, line: String },
  ExitStatus { id: String, code: i32 },
  Error { id: String, message: String },
  Shutdown { message: String },
}

#[cfg(test)]
mod tests {
  use super::{OctaCommand, Schema};

  #[test]
  fn schema_without_validation_schema_is_backward_compatible() {
    let schema: Schema = serde_json::from_str(r#"{"key":"shell"}"#).unwrap();

    assert_eq!(schema.key, "shell");
    assert!(schema.capabilities.is_empty());
    assert!(schema.validation_schema.is_none());
    assert_eq!(serde_json::to_string(&schema).unwrap(), r#"{"key":"shell"}"#);
  }

  #[test]
  fn boolean_validation_schema_is_rejected() {
    let result = serde_json::from_str::<Schema>(r#"{"key":"shell","validation_schema":true}"#);

    assert!(result.is_err());
  }

  #[test]
  fn execute_without_secret_vars_is_backward_compatible() {
    let command: OctaCommand = serde_json::from_str(
      r#"{"type":"Execute","payload":{"params":"echo","args":[],"dir":".","envs":{},"vars":{},"dry":false}}"#,
    )
    .unwrap();

    let OctaCommand::Execute {
      secret_vars,
      redact_params,
      ..
    } = command
    else {
      panic!("expected execute command");
    };
    assert!(secret_vars.is_empty());
    assert!(!redact_params);
  }
}
