use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Version {
  pub version: String,
  pub features: Vec<String>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "payload")]
pub enum ClientCommand {
  Hello(Version),
  Execute {
    command: String,
    args: Vec<String>,
    dir: PathBuf,
    envs: HashMap<String, String>,
  },
  Shutdown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ServerResponse {
  Hello(Version),
  Started { id: String },
  Stdout { id: String, line: String },
  Stderr { id: String, line: String },
  ExitStatus { id: String, code: i32 },
  Error { id: String, message: String },
  Shutdown { message: String },
}
