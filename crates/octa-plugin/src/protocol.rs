//! Private versioned wire messages exchanged by Octa and execution plugins.

use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Current engine-to-plugin wire protocol.
///
/// Version two adds side-effect-free filesystem contract planning and keeps
/// plugin parameters as typed JSON values. Octa does not negotiate version-one
/// compatibility because a host must know whether a missing plan means
/// "opaque" rather than "old plugin" before cache lookup.
pub const PLUGIN_PROTOCOL_VERSION: u16 = 2;

/// Maximum total number of input patterns and output roots in one plugin plan.
pub const MAX_PLUGIN_CACHE_PLAN_ITEMS: usize = 1_024;
/// Maximum UTF-8 length of one target label, input pattern, or output root.
pub const MAX_PLUGIN_CACHE_PLAN_STRING_BYTES: usize = 4_096;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Version {
  pub protocol_version: u16,
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
  /// JSON Schema for values accepted in `Execute.params` and `PlanCache.params`.
  pub input_schema: Option<Map<String, Value>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  /// JSON Schema for the object returned in a successful `Completed.outputs`.
  pub output_schema: Option<Map<String, Value>>,
}

/// Execution platform for which a plugin computes a filesystem contract.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct TargetPlatform {
  /// Normalized operating-system name such as `linux`, `macos`, or `windows`.
  pub os: String,
  /// Normalized architecture name such as `x86_64` or `arm64`.
  pub architecture: String,
}

/// Immutable inputs supplied to a plugin's side-effect-free planning method.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginCachePlanRequest {
  /// Validated plugin task value without the legacy string encoding used by execution.
  pub params: Value,
  /// Portable task working directory relative to the workspace; empty is root.
  pub working_directory: String,
  /// Platform on which the later command will execute.
  pub target: TargetPlatform,
}

/// Complete filesystem contract required by one plugin invocation.
///
/// Both path classes are workspace-relative. Input patterns retain their
/// ordered include/exclude semantics within this plan. Outputs are exact file
/// or directory roots. Returning this value asserts completeness; returning
/// `CachePlanUnavailable` declares the invocation opaque.
#[derive(Serialize, Deserialize, Debug, Clone, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PluginCachePlan {
  /// Ordered workspace-relative input patterns required by the invocation.
  #[serde(default)]
  pub inputs: Vec<String>,
  /// Exact workspace-relative output roots owned by the invocation.
  #[serde(default)]
  pub outputs: Vec<String>,
}

impl PluginCachePlan {
  /// Enforces transport bounds before the plan reaches cache path validation.
  pub fn validate(&self) -> Result<(), String> {
    let items = self
      .inputs
      .len()
      .checked_add(self.outputs.len())
      .ok_or_else(|| "plugin cache plan item count overflowed".to_owned())?;
    if items > MAX_PLUGIN_CACHE_PLAN_ITEMS {
      return Err(format!(
        "plugin cache plan is limited to {MAX_PLUGIN_CACHE_PLAN_ITEMS} input and output items"
      ));
    }
    for (kind, values) in [("input pattern", &self.inputs), ("output root", &self.outputs)] {
      for value in values {
        if value.is_empty() || value.len() > MAX_PLUGIN_CACHE_PLAN_STRING_BYTES {
          return Err(format!(
            "plugin cache plan {kind} must contain 1..={MAX_PLUGIN_CACHE_PLAN_STRING_BYTES} UTF-8 bytes"
          ));
        }
        if value.chars().any(char::is_control) {
          return Err(format!("plugin cache plan {kind} must not contain control characters"));
        }
      }
    }
    Ok(())
  }
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

/// Artifact path reported by a plugin, relative to the command working directory.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct ArtifactDeclaration {
  pub name: String,
  pub path: PathBuf,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub content_type: Option<String>,
}

/// Report path reported by a plugin, relative to the command working directory.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub struct ReportDeclaration {
  pub name: String,
  pub path: PathBuf,
  /// Stable format identifier owned by the reporting plugin, for example `junit`.
  pub format: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "payload")]
pub enum OctaCommand {
  Hello(Version),
  Schema,
  /// Requests a complete filesystem contract without executing the command.
  PlanCache {
    /// Host-assigned identity echoed by the planning response.
    id: String,
    /// Validated command parameters and immutable planning context.
    request: PluginCachePlanRequest,
  },
  Execute {
    /// Host-assigned identity echoed by every response for this command.
    id: String,
    /// Validated plugin task value in its original JSON type.
    params: Value,
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
  /// Complete filesystem contract for one planned plugin invocation.
  CachePlan {
    /// Identity copied from `PlanCache`.
    id: String,
    /// Required inputs and owned outputs for the later execution.
    plan: PluginCachePlan,
  },
  /// The plugin cannot completely describe this invocation's filesystem use.
  CachePlanUnavailable {
    /// Identity copied from `PlanCache`.
    id: String,
  },
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
  RegisterArtifact {
    id: String,
    artifact: ArtifactDeclaration,
  },
  RegisterReport {
    id: String,
    report: ReportDeclaration,
  },
  /// Normal terminal result of a plugin operation.
  Completed {
    id: String,
    /// Process-compatible status where zero represents success.
    code: i32,
    /// Structured values produced by the completed operation.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    outputs: Map<String, Value>,
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
  use super::*;

  fn request() -> PluginCachePlanRequest {
    PluginCachePlanRequest {
      params: serde_json::json!({ "file": "templates/app.txt" }),
      working_directory: "project".to_owned(),
      target: TargetPlatform {
        os: "linux".to_owned(),
        architecture: "x86_64".to_owned(),
      },
    }
  }

  #[test]
  fn cache_planning_messages_round_trip_with_the_request_identity() {
    let command = OctaCommand::PlanCache {
      id: "plan-1".to_owned(),
      request: request(),
    };
    let encoded = serde_json::to_value(command).unwrap();
    let decoded = serde_json::from_value::<OctaCommand>(encoded).unwrap();
    assert!(matches!(
      decoded,
      OctaCommand::PlanCache { id, request: value }
        if id == "plan-1" && value == request()
    ));

    let response = PluginResponse::CachePlan {
      id: "plan-1".to_owned(),
      plan: PluginCachePlan {
        inputs: vec!["project/src/**".to_owned()],
        outputs: vec!["project/target".to_owned()],
      },
    };
    let encoded = serde_json::to_value(response).unwrap();
    let decoded = serde_json::from_value::<PluginResponse>(encoded).unwrap();
    assert!(matches!(
      decoded,
      PluginResponse::CachePlan { id, plan }
        if id == "plan-1" && plan.inputs == ["project/src/**"] && plan.outputs == ["project/target"]
    ));
  }

  #[test]
  fn cache_plan_request_rejects_unknown_fields_and_plan_limits_are_bounded() {
    let mut encoded = serde_json::to_value(request()).unwrap();
    encoded["unknown"] = Value::Bool(true);
    assert!(serde_json::from_value::<PluginCachePlanRequest>(encoded).is_err());

    let oversized = PluginCachePlan {
      inputs: vec!["input".to_owned(); MAX_PLUGIN_CACHE_PLAN_ITEMS + 1],
      outputs: Vec::new(),
    };
    assert!(oversized.validate().unwrap_err().contains("limited"));
    let invalid = PluginCachePlan {
      inputs: vec!["bad\npattern".to_owned()],
      outputs: Vec::new(),
    };
    assert!(invalid.validate().unwrap_err().contains("control"));
    for invalid in [String::new(), "x".repeat(MAX_PLUGIN_CACHE_PLAN_STRING_BYTES + 1)] {
      let invalid = PluginCachePlan {
        inputs: Vec::new(),
        outputs: vec![invalid],
      };
      assert!(invalid.validate().unwrap_err().contains("UTF-8 bytes"));
    }
  }

  #[test]
  fn schema_without_optional_schemas_uses_no_validation() {
    let schema: Schema = serde_json::from_str(r#"{"key":"shell"}"#).unwrap();

    assert_eq!(schema.key, "shell");
    assert!(schema.capabilities.is_empty());
    assert!(!schema.supports_raw);
    assert!(schema.input_schema.is_none());
    assert!(schema.output_schema.is_none());
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
  fn completed_response_carries_structured_outputs() {
    let response = PluginResponse::Completed {
      id: "command".to_owned(),
      code: 0,
      outputs: serde_json::Map::from_iter([
        ("digest".to_owned(), serde_json::json!("sha256:test")),
        ("pushed".to_owned(), serde_json::json!(true)),
      ]),
    };
    let json = serde_json::to_string(&response).unwrap();
    let decoded = serde_json::from_str::<PluginResponse>(&json).unwrap();

    assert!(matches!(
      decoded,
      PluginResponse::Completed { id, code: 0, outputs }
        if id == "command"
          && outputs["digest"] == "sha256:test"
          && outputs["pushed"] == true
    ));
  }

  #[test]
  fn report_format_is_owned_by_the_plugin() {
    let response = PluginResponse::RegisterReport {
      id: "command".to_owned(),
      report: ReportDeclaration {
        name: "benchmark".to_owned(),
        path: "reports/result.json".into(),
        format: "acme/benchmark-v2".to_owned(),
      },
    };
    let json = serde_json::to_string(&response).unwrap();
    let decoded = serde_json::from_str::<PluginResponse>(&json).unwrap();

    assert!(matches!(
      decoded,
      PluginResponse::RegisterReport { report, .. } if report.format == "acme/benchmark-v2"
    ));
  }

  #[test]
  fn boolean_input_schema_is_rejected() {
    let result = serde_json::from_str::<Schema>(r#"{"key":"shell","input_schema":true}"#);

    assert!(result.is_err());
  }

  #[test]
  fn execute_defaults_optional_fields_without_changing_the_host_id() {
    let command: OctaCommand = serde_json::from_str(
      r#"{"type":"Execute","payload":{"id":"host-command","params":"echo","args":[],"dir":".","envs":{},"vars":{},"dry":false}}"#,
    )
    .unwrap();

    let OctaCommand::Execute {
      id,
      secret_vars,
      redact_params,
      raw,
      ..
    } = command
    else {
      panic!("expected execute command");
    };
    assert_eq!(id, "host-command");
    assert!(secret_vars.is_empty());
    assert!(!redact_params);
    assert!(!raw);
  }

  #[test]
  fn execute_preserves_structured_parameters_without_string_encoding() {
    let command = OctaCommand::Execute {
      id: "structured".to_owned(),
      params: serde_json::json!({ "file": "report.xml" }),
      args: Vec::new(),
      dir: PathBuf::from("."),
      envs: HashMap::new(),
      vars: HashMap::new(),
      secret_vars: Vec::new(),
      redact_params: false,
      raw: false,
      dry: false,
    };
    let encoded = serde_json::to_value(command).unwrap();
    assert_eq!(
      encoded["payload"]["params"],
      serde_json::json!({ "file": "report.xml" })
    );
  }

  #[test]
  fn execute_requires_a_host_assigned_id() {
    let command = serde_json::from_str::<OctaCommand>(
      r#"{"type":"Execute","payload":{"params":"echo","args":[],"dir":".","envs":{},"vars":{},"dry":false}}"#,
    );

    assert!(command.is_err());
  }
}
