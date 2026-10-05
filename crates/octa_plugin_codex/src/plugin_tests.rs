use std::{collections::HashMap, fs, path::Path};

use octa_plugin::{
  logger::MockLogger,
  protocol::{PluginCachePlanRequest, PluginResponse, TargetPlatform},
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, DuplexStream};

use super::*;

fn command(workspace: &Path, value: Value, vars: HashMap<String, Value>) -> PluginCommand {
  PluginCommand {
    id: "command-id".to_owned(),
    dry: true,
    value,
    args: Vec::new(),
    dir: workspace.to_owned(),
    vars,
    secret_vars: vec!["credential".to_owned()],
    envs: HashMap::from([("PATH".to_owned(), workspace.join("missing-bin").display().to_string())]),
    raw: false,
    input: tokio::sync::mpsc::unbounded_channel().1,
  }
}

async fn execute(command: PluginCommand) -> anyhow::Result<Vec<PluginResponse>> {
  let (stream, mut reader) = tokio::io::duplex(4096);
  CodexPlugin
    .execute_command(
      command,
      Arc::new(Mutex::new(stream)),
      Arc::new(MockLogger::new()),
      CancellationToken::new(),
    )
    .await?;
  read_responses(&mut reader).await
}

async fn read_responses(reader: &mut DuplexStream) -> anyhow::Result<Vec<PluginResponse>> {
  let mut bytes = Vec::new();
  reader.read_to_end(&mut bytes).await?;
  String::from_utf8(bytes)?
    .lines()
    .map(|line| serde_json::from_str(line).map_err(Into::into))
    .collect()
}

#[test]
fn exposes_the_codex_task_contract_without_raw_mode() {
  let schema = plugin_schema();

  assert_eq!(schema.key, "codex");
  assert!(!schema.supports_raw);
  assert!(schema.capabilities.is_empty());
  assert!(schema.input_schema.is_some());
  assert!(schema.output_schema.is_some());
}

#[test]
fn exposes_the_package_version() {
  assert_eq!(CodexPlugin.version(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn reports_no_automatic_cache_contract() {
  let request = PluginCachePlanRequest {
    params: json!({
      "prompt_file": "missing/prompt.md",
      "deliverables": [{ "kind": "artifact", "name": "patch", "path": "out/patch.diff" }]
    }),
    working_directory: "project".to_owned(),
    target: TargetPlatform {
      os: "linux".to_owned(),
      architecture: "x86_64".to_owned(),
    },
  };

  assert!(CodexPlugin.cache_plan(&request).unwrap().is_none());
}

#[tokio::test]
async fn dry_run_validates_without_reading_execution_material_or_mutating_the_workspace() {
  let workspace = tempfile::tempdir().unwrap();
  let secret = "must-not-appear";
  let responses = execute(command(
    workspace.path(),
    json!({
      "prompt_file": "missing/prompt.md",
      "run_records": ".octa/codex-runs",
      "environment": { "secret": { "OPENAI_API_KEY": "credential" } }
    }),
    HashMap::from([("credential".to_owned(), json!(secret))]),
  ))
  .await
  .unwrap();

  let [PluginResponse::Completed { id, code: 0, outputs }] = responses.as_slice() else {
    panic!("unexpected dry-run responses: {responses:?}");
  };
  assert_eq!(id, "command-id");
  assert_eq!(outputs, &records::dry_run_outputs());
  assert!(jsonschema::validator_for(&Value::Object(records::output_schema()))
    .unwrap()
    .is_valid(&Value::Object(outputs.clone())));
  assert!(!format!("{responses:?}").contains(secret));
  assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn dry_run_rejects_an_invalid_static_configuration_before_execution() {
  let workspace = tempfile::tempdir().unwrap();
  let error = execute(command(
    workspace.path(),
    json!({ "prompt": "inline", "prompt_file": "prompt.md" }),
    HashMap::new(),
  ))
  .await
  .unwrap_err();

  assert!(error.to_string().contains("exactly one of prompt or prompt_file"));
  assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
}
