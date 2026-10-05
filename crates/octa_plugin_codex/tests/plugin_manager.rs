use std::{collections::HashMap, fs, path::Path};

use octa_plugin::protocol::PluginResponse;
use octa_plugin_manager::{plugin_client::PluginExecutionRequest, plugin_manager::PluginManager};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn plugin_manager(workspace: &Path) -> (PluginManager, String) {
  let executable = Path::new(env!("CARGO_BIN_EXE_octa_plugin_codex"));
  let directory = executable.parent().expect("plugin executable must have a parent");
  let name = executable
    .file_name()
    .expect("plugin executable must have a file name")
    .to_string_lossy()
    .into_owned();
  (PluginManager::with_workspace(directory, workspace), name)
}

fn install_process_fixture(directory: &Path) -> std::path::PathBuf {
  let source = Path::new(env!("CARGO_BIN_EXE_codex-test-fixture"));
  let target = directory.join(format!("codex{}", std::env::consts::EXE_SUFFIX));
  fs::copy(source, &target).expect("copy Codex process fixture");
  target
}

fn dry_run_request(workspace: &Path, fixture_directory: &Path) -> PluginExecutionRequest {
  PluginExecutionRequest {
    params: json!({
      "prompt_file": "missing/prompt.md",
      "environment": { "secret": { "OPENAI_API_KEY": "credential" } }
    }),
    dry: true,
    args: Vec::new(),
    dir: workspace.to_owned(),
    vars: HashMap::from([("credential".to_owned(), json!("must-not-appear"))]),
    envs: HashMap::from([("PATH".to_owned(), fixture_directory.display().to_string())]),
    secret_vars: vec!["credential".to_owned()],
    redact_params: true,
    raw: false,
  }
}

#[tokio::test]
async fn dry_run_uses_the_registered_contract_without_spawning_codex() {
  let workspace = tempfile::tempdir().unwrap();
  let fixture_directory = tempfile::tempdir().unwrap();
  let fixture = install_process_fixture(fixture_directory.path());
  let marker = fixture.with_file_name("spawned");
  let (manager, plugin_name) = plugin_manager(workspace.path());

  manager.start_plugin(&plugin_name).await.unwrap();
  let registration = manager.resolve_key("codex").await.unwrap();
  let client = manager.get_client("codex").await.unwrap();
  let mut execution = client
    .start_execution(
      dry_run_request(workspace.path(), fixture_directory.path()),
      CancellationToken::new(),
    )
    .await
    .unwrap();

  let response = execution.receive_output(&CancellationToken::new()).await.unwrap();
  let Some(PluginResponse::Completed { code: 0, outputs, .. }) = response else {
    panic!("unexpected dry-run response: {response:?}");
  };
  registration.validate_outputs(&Value::Object(outputs.clone())).unwrap();
  assert_eq!(outputs, json!({ "outcome": "completed" }).as_object().unwrap().clone());
  assert!(!marker.exists(), "dry run spawned the selected Codex fixture");
  assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());

  let normal_output = json!({
    "outcome": "blocked",
    "final_message": "credential unavailable",
    "harness_identifiers": {},
    "usage": {},
    "record_paths": {
      "trace": ".octa/codex-runs/run/trace.jsonl",
      "result": ".octa/codex-runs/run/result.json",
      "provenance": ".octa/codex-runs/run/provenance.json"
    }
  });
  registration.validate_outputs(&normal_output).unwrap();

  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}
