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
  /// Whether this plugin can run commands inside an interactive PTY.
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  pub supports_raw: bool,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub capabilities: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub validation_schema: Option<Map<String, Value>>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticLevel {
  Trace,
  Debug,
  Info,
  Warn,
  Error,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SourceLocation {
  pub file: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub line: Option<u64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub column: Option<u64>,
}

/// Transient command progress. It is intentionally separate from stdout/stderr.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct ProgressUpdate {
  /// Human-readable description of the current activity.
  pub message: String,
  /// Completed amount, when the operation exposes a measurable position.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub current: Option<u64>,
  /// Total amount, when it is known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub total: Option<u64>,
  /// Unit for `current` and `total`, for example `files` or `bytes`.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub unit: Option<String>,
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
    /// Requests byte-oriented output suitable for an exclusive terminal session.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    raw: bool,
    dry: bool,
  },
  Cancel {
    id: String,
  },
  Stdin {
    id: String,
    #[serde(with = "base64_bytes")]
    bytes: Vec<u8>,
  },
  Resize {
    id: String,
    rows: u16,
    cols: u16,
  },
  CloseStdin {
    id: String,
  },
  Shutdown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "payload")]
pub enum PluginResponse {
  Hello(Version),
  Schema(Schema),
  Started {
    id: String,
  },
  Stdout {
    id: String,
    line: String,
  },
  Stderr {
    id: String,
    line: String,
  },
  StdoutBytes {
    id: String,
    #[serde(with = "base64_bytes")]
    bytes: Vec<u8>,
  },
  StderrBytes {
    id: String,
    #[serde(with = "base64_bytes")]
    bytes: Vec<u8>,
  },
  Diagnostic {
    id: String,
    level: DiagnosticLevel,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location: Option<SourceLocation>,
  },
  Progress {
    id: String,
    progress: ProgressUpdate,
  },
  ExitStatus {
    id: String,
    code: i32,
  },
  Error {
    id: String,
    message: String,
  },
  Shutdown {
    message: String,
  },
}

mod base64_bytes {
  use base64::{engine::general_purpose::STANDARD, Engine as _};
  use serde::{de::Error as _, Deserialize, Deserializer, Serializer};

  pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    serializer.serialize_str(&STANDARD.encode(bytes))
  }

  pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
  where
    D: Deserializer<'de>,
  {
    let encoded = String::deserialize(deserializer)?;
    STANDARD.decode(encoded).map_err(D::Error::custom)
  }
}

#[cfg(test)]
mod tests {
  use super::{OctaCommand, PluginResponse, ProgressUpdate, Schema};

  #[test]
  fn schema_without_validation_schema_is_backward_compatible() {
    let schema: Schema = serde_json::from_str(r#"{"key":"shell"}"#).unwrap();

    assert_eq!(schema.key, "shell");
    assert!(schema.capabilities.is_empty());
    assert!(!schema.supports_raw);
    assert!(schema.validation_schema.is_none());
    assert_eq!(serde_json::to_string(&schema).unwrap(), r#"{"key":"shell"}"#);
  }

  #[test]
  fn byte_payloads_are_base64_strings_instead_of_json_integer_arrays() {
    let response = PluginResponse::StdoutBytes {
      id: "command".to_owned(),
      bytes: vec![0, 1, 255],
    };
    let json = serde_json::to_string(&response).unwrap();

    assert!(json.contains(r#""bytes":"AAH/""#));
    let PluginResponse::StdoutBytes { bytes, .. } = serde_json::from_str(&json).unwrap() else {
      panic!("expected bytes response");
    };
    assert_eq!(bytes, [0, 1, 255]);
  }

  #[test]
  fn structured_progress_round_trips_without_output_payloads() {
    let response = PluginResponse::Progress {
      id: "command".to_owned(),
      progress: ProgressUpdate {
        message: "Compiling".to_owned(),
        current: Some(3),
        total: Some(10),
        unit: Some("files".to_owned()),
      },
    };
    let json = serde_json::to_string(&response).unwrap();
    let decoded = serde_json::from_str::<PluginResponse>(&json).unwrap();

    assert!(matches!(
      decoded,
      PluginResponse::Progress {
        id,
        progress: ProgressUpdate {
          current: Some(3),
          total: Some(10),
          ..
        }
      } if id == "command"
    ));
    assert!(!json.contains("stdout"));
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
      raw,
      ..
    } = command
    else {
      panic!("expected execute command");
    };
    assert!(secret_vars.is_empty());
    assert!(!redact_params);
    assert!(!raw);
  }
}
