use std::{
  collections::HashMap,
  fs,
  path::{Path, PathBuf},
  sync::Arc,
  time::Duration,
};

use async_trait::async_trait;
use octa_plugin::protocol::PluginResponse;
use octa_plugin_manager::{
  plugin_client::PluginExecutionRequest,
  plugin_manager::PluginManager,
  plugin_process::{LocalPluginLauncher, PluginLaunchError, PluginLaunchRequest, PluginLauncher, PluginProcess},
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

fn plugin_artifact() -> (&'static Path, String) {
  let executable = Path::new(env!("CARGO_BIN_EXE_octa_plugin_codex"));
  let directory = executable.parent().expect("plugin executable must have a parent");
  let name = executable
    .file_name()
    .expect("plugin executable must have a file name")
    .to_string_lossy()
    .into_owned();
  (directory, name)
}

fn plugin_manager(workspace: &Path) -> (PluginManager, String) {
  let (directory, name) = plugin_artifact();
  (PluginManager::with_workspace(directory, workspace), name)
}

struct OperatorEnvironmentLauncher {
  codex_executable: PathBuf,
}

#[async_trait]
impl PluginLauncher for OperatorEnvironmentLauncher {
  async fn launch(&self, mut request: PluginLaunchRequest) -> Result<PluginProcess, PluginLaunchError> {
    request.environment.insert(
      "OCTA_CODEX_EXECUTABLE".to_owned(),
      self.codex_executable.to_string_lossy().into_owned(),
    );
    LocalPluginLauncher.launch(request).await
  }
}

fn plugin_manager_with_codex(workspace: &Path, codex_executable: PathBuf) -> (PluginManager, String) {
  let (directory, name) = plugin_artifact();
  let launcher = Arc::new(OperatorEnvironmentLauncher { codex_executable });
  (PluginManager::with_launcher(directory, workspace, launcher), name)
}

fn install_process_fixture(directory: &Path) -> std::path::PathBuf {
  let source = Path::new(env!("CARGO_BIN_EXE_codex-test-fixture"));
  let target = directory.join(format!("codex{}", std::env::consts::EXE_SUFFIX));
  fs::copy(source, &target).expect("copy Codex process fixture");
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).expect("make Codex process fixture executable");
  }
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

fn compatibility_request(workspace: &Path, path_directory: &Path) -> PluginExecutionRequest {
  PluginExecutionRequest {
    params: json!({ "prompt_file": "must-not-be-read.md" }),
    dry: false,
    args: Vec::new(),
    dir: workspace.to_owned(),
    vars: HashMap::new(),
    envs: HashMap::from([("PATH".to_owned(), path_directory.display().to_string())]),
    secret_vars: Vec::new(),
    redact_params: true,
    raw: false,
  }
}

async fn compatibility_response(workspace: &Path, selected: PathBuf, path_directory: &Path) -> PluginResponse {
  compatibility_response_with_cancellation(workspace, selected, path_directory, CancellationToken::new()).await
}

async fn compatibility_response_with_cancellation(
  workspace: &Path,
  selected: PathBuf,
  path_directory: &Path,
  cancellation: CancellationToken,
) -> PluginResponse {
  let (manager, plugin_name) = plugin_manager_with_codex(workspace, selected);
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("codex").await.unwrap();
  let mut execution = client
    .start_execution(compatibility_request(workspace, path_directory), cancellation)
    .await
    .unwrap();
  let response = tokio::time::timeout(Duration::from_secs(10), async {
    loop {
      match execution.receive_output(&CancellationToken::new()).await.unwrap() {
        Some(response @ (PluginResponse::Completed { .. } | PluginResponse::Error { .. })) => return response,
        Some(_) => {},
        None => panic!("plugin response stream closed before a terminal response"),
      }
    }
  })
  .await
  .expect("compatibility execution timed out");
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
  response
}

async fn compatibility_error(workspace: &Path, selected: PathBuf, path_directory: &Path) -> String {
  let response = compatibility_response(workspace, selected, path_directory).await;
  let PluginResponse::Error { message, .. } = response else {
    panic!("unexpected compatibility response: {response:?}");
  };
  message
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

#[tokio::test]
async fn compatibility_probe_fails_closed_without_path_or_shell_fallback() {
  let workspace = tempfile::tempdir().unwrap();
  let path_directory = tempfile::tempdir().unwrap();
  let fallback = install_process_fixture(path_directory.path());

  let missing = workspace
    .path()
    .join(format!("missing-codex{}", std::env::consts::EXE_SUFFIX));
  let message = compatibility_error(workspace.path(), missing, path_directory.path()).await;
  assert!(message.contains("does not exist"), "unexpected error: {message}");
  assert!(message.len() < 512, "unbounded compatibility error: {message}");
  assert!(
    !fallback.with_extension("version-probed").exists(),
    "missing selection fell back to a Codex executable from PATH"
  );

  let selected_directory = tempfile::tempdir().unwrap();
  let message = compatibility_error(
    workspace.path(),
    selected_directory.path().to_owned(),
    path_directory.path(),
  )
  .await;
  assert!(message.contains("regular file"), "unexpected error: {message}");
  assert!(message.len() < 512, "unbounded compatibility error: {message}");

  let non_executable = workspace.path().join(if cfg!(windows) {
    "codex-not-executable.txt"
  } else {
    "codex-not-executable"
  });
  fs::copy(Path::new(env!("CARGO_BIN_EXE_codex-test-fixture")), &non_executable).unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&non_executable, fs::Permissions::from_mode(0o644)).unwrap();
  }
  let message = compatibility_error(workspace.path(), non_executable, path_directory.path()).await;
  assert!(
    message.contains("executable") || message.contains("native .exe"),
    "unexpected error: {message}"
  );
  assert!(message.len() < 512, "unbounded compatibility error: {message}");

  let oversized_response = "x".repeat(4 * 1024 + 1);
  for (response, expected) in [
    ("not a version\n", "malformed Codex version output"),
    ("codex-cli 99.0.0\n", "version 99.0.0 is not supported"),
    (oversized_response.as_str(), "output exceeded"),
  ] {
    let directory = tempfile::tempdir().unwrap();
    let selected = install_process_fixture(directory.path());
    fs::write(selected.with_extension("version"), response).unwrap();
    let message = compatibility_error(workspace.path(), selected, path_directory.path()).await;
    assert!(message.contains(expected), "unexpected error: {message}");
    assert!(message.len() < 512, "unbounded compatibility error: {message}");
  }

  let supported_directory = tempfile::tempdir().unwrap();
  let supported = install_process_fixture(supported_directory.path());
  let response = compatibility_response(workspace.path(), supported.clone(), path_directory.path()).await;
  let PluginResponse::Error { message, .. } = response else {
    panic!("supported compatibility probe returned an unexpected response: {response:?}");
  };
  assert_eq!(
    message,
    "Command execution error: Codex task contract is not implemented"
  );
  assert!(supported.with_extension("version-probed").exists());

  assert!(fs::read_dir(workspace.path()).unwrap().all(|entry| {
    let name = entry.unwrap().file_name();
    name.to_string_lossy().starts_with("codex-not-executable")
  }));
}

#[tokio::test]
async fn compatibility_probe_owns_descendants_and_honors_cancellation() {
  let workspace = tempfile::tempdir().unwrap();
  let path_directory = tempfile::tempdir().unwrap();

  let descendant_directory = tempfile::tempdir().unwrap();
  let with_descendant = install_process_fixture(descendant_directory.path());
  fs::write(with_descendant.with_extension("spawn-version-descendant"), b"").unwrap();
  let response = compatibility_response(workspace.path(), with_descendant.clone(), path_directory.path()).await;
  assert!(
    matches!(response, PluginResponse::Error { message, .. } if message == "Command execution error: Codex task contract is not implemented")
  );
  let heartbeat = with_descendant.with_extension("version-descendant-heartbeat");
  let before = fs::read_to_string(&heartbeat).unwrap();
  tokio::time::sleep(Duration::from_millis(100)).await;
  let after = fs::read_to_string(&heartbeat).unwrap();
  assert_eq!(before, after, "version-probe descendant survived its leader");

  let cancelled_directory = tempfile::tempdir().unwrap();
  let cancelled = install_process_fixture(cancelled_directory.path());
  fs::write(cancelled.with_extension("hang-version"), b"").unwrap();
  let (manager, plugin_name) = plugin_manager_with_codex(workspace.path(), cancelled.clone());
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("codex").await.unwrap();
  let mut execution = client
    .start_execution(
      compatibility_request(workspace.path(), path_directory.path()),
      CancellationToken::new(),
    )
    .await
    .unwrap();
  for _ in 0..200 {
    if cancelled.with_extension("version-probed").exists() {
      break;
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
  }
  assert!(cancelled.with_extension("version-probed").exists());
  tokio::time::timeout(Duration::from_secs(1), execution.cancel_and_wait())
    .await
    .expect("cancelled version probe did not terminate promptly")
    .unwrap();
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}
