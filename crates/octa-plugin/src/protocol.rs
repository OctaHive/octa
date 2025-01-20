use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Version {
  pub version: String,
  pub features: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Schema {
  pub key: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "payload")]
pub enum OctaCommand {
  Hello(Version),
  Schema,
  Execute {
    command: String,
    args: Vec<String>,
    dir: PathBuf,
    envs: HashMap<String, String>,
    vars: HashMap<String, Value>,
    dry: bool,
  },
  Shutdown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
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
