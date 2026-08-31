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
  use super::Schema;

  #[test]
  fn schema_without_validation_schema_is_backward_compatible() {
    let schema: Schema = serde_json::from_str(r#"{"key":"shell"}"#).unwrap();

    assert_eq!(schema.key, "shell");
    assert!(schema.validation_schema.is_none());
    assert_eq!(serde_json::to_string(&schema).unwrap(), r#"{"key":"shell"}"#);
  }

  #[test]
  fn boolean_validation_schema_is_rejected() {
    let result = serde_json::from_str::<Schema>(r#"{"key":"shell","validation_schema":true}"#);

    assert!(result.is_err());
  }
}
