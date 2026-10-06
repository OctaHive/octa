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
  plugin_client::{PluginExecution, PluginExecutionRequest},
  plugin_manager::PluginManager,
  plugin_process::{LocalPluginLauncher, PluginLaunchError, PluginLaunchRequest, PluginLauncher, PluginProcess},
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

mod support;

use support::{configured_process_fixture, install_process_fixture};

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

fn lifecycle_request(workspace: &Path) -> PluginExecutionRequest {
  PluginExecutionRequest {
    params: json!({ "prompt": "perform the fixture task" }),
    dry: false,
    args: Vec::new(),
    dir: workspace.to_owned(),
    vars: HashMap::new(),
    envs: HashMap::new(),
    secret_vars: Vec::new(),
    redact_params: true,
    raw: false,
  }
}

async fn start_lifecycle_execution(workspace: &Path, fixture: PathBuf) -> (PluginManager, PluginExecution) {
  let (manager, plugin_name) = plugin_manager_with_codex(workspace, fixture);
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("codex").await.unwrap();
  let execution = client
    .start_execution(lifecycle_request(workspace), CancellationToken::new())
    .await
    .unwrap();
  (manager, execution)
}

async fn receive_terminal(execution: &mut PluginExecution) -> PluginResponse {
  receive_through_terminal(execution)
    .await
    .pop()
    .expect("a terminal response was collected")
}

async fn receive_through_terminal(execution: &mut PluginExecution) -> Vec<PluginResponse> {
  tokio::time::timeout(Duration::from_secs(10), async {
    let mut responses = Vec::new();
    loop {
      match execution.receive_output(&CancellationToken::new()).await.unwrap() {
        Some(response) => {
          let terminal = matches!(
            response,
            PluginResponse::Completed { .. } | PluginResponse::Error { .. }
          );
          responses.push(response);
          if terminal {
            return responses;
          }
        },
        None => panic!("plugin response stream closed before a terminal response"),
      }
    }
  })
  .await
  .expect("Codex execution timed out")
}

async fn assert_response_stream_closed(execution: &mut PluginExecution) {
  let response = tokio::time::timeout(
    Duration::from_secs(1),
    execution.receive_output(&CancellationToken::new()),
  )
  .await
  .expect("plugin response route remained open after its terminal response")
  .expect("plugin response route failed after its terminal response");
  assert!(
    response.is_none(),
    "plugin emitted a second response after terminal: {response:?}"
  );
}

async fn wait_for_file(path: &Path) {
  tokio::time::timeout(Duration::from_secs(5), async {
    while !path.exists() {
      tokio::time::sleep(Duration::from_millis(10)).await;
    }
  })
  .await
  .unwrap_or_else(|_| panic!("fixture did not create {}", path.display()));
}

async fn assert_heartbeat_stopped(path: &Path) {
  let before = fs::read_to_string(path).unwrap();
  tokio::time::sleep(Duration::from_millis(100)).await;
  let after = fs::read_to_string(path).unwrap();
  assert_eq!(before, after, "fixture process survived lifecycle teardown");
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
    assert!(
      message.contains(expected),
      "expected {expected:?}, unexpected error: {message}"
    );
    assert!(message.len() < 512, "unbounded compatibility error: {message}");
  }

  let supported_directory = tempfile::tempdir().unwrap();
  let supported = install_process_fixture(supported_directory.path());
  let response = compatibility_response(workspace.path(), supported.clone(), path_directory.path()).await;
  let PluginResponse::Error { message, .. } = response else {
    panic!("supported compatibility probe returned an unexpected response: {response:?}");
  };
  assert!(message.contains("prompt_file"), "unexpected error: {message}");
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
  assert!(matches!(response, PluginResponse::Error { message, .. } if message.contains("prompt_file")));
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

#[tokio::test]
async fn cancellation_during_output_stops_the_tree_and_returns_one_terminal_response() {
  let workspace = tempfile::tempdir().unwrap();
  let fixture_directory = tempfile::tempdir().unwrap();
  let fixture = configured_process_fixture(fixture_directory.path(), "hang");
  let heartbeat = fixture.with_extension("run-heartbeat");
  let (manager, mut execution) = start_lifecycle_execution(workspace.path(), fixture).await;
  wait_for_file(&heartbeat).await;

  tokio::time::timeout(Duration::from_secs(5), execution.cancel_and_wait())
    .await
    .expect("cancellation did not complete")
    .unwrap();
  let response = receive_terminal(&mut execution).await;
  assert!(matches!(response, PluginResponse::Completed { code: -1, .. }));
  assert_response_stream_closed(&mut execution).await;
  assert_heartbeat_stopped(&heartbeat).await;
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn exit_racing_with_cancellation_produces_one_terminal_response() {
  let workspace = tempfile::tempdir().unwrap();
  let fixture_directory = tempfile::tempdir().unwrap();
  let fixture = configured_process_fixture(fixture_directory.path(), "exit-race");
  let ready = fixture.with_extension("run-heartbeat");
  let release = fixture.with_extension("release-run");
  let (manager, mut execution) = start_lifecycle_execution(workspace.path(), fixture).await;
  wait_for_file(&ready).await;

  fs::write(release, b"release").unwrap();
  tokio::time::timeout(Duration::from_secs(5), execution.cancel_and_wait())
    .await
    .expect("exit-versus-cancel race did not settle")
    .unwrap();
  let terminal = receive_terminal(&mut execution).await;
  // Releasing the fixture and cancelling are intentionally concurrent. A
  // fully observed semantic completion may win before the cancellation token;
  // otherwise the plugin reports cancellation (or a bounded transport error).
  assert!(matches!(
    terminal,
    PluginResponse::Completed { code: -1 | 0, .. } | PluginResponse::Error { .. }
  ));
  assert_response_stream_closed(&mut execution).await;
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn cancellation_timeout_forces_an_uncooperative_process_tree_down() {
  let workspace = tempfile::tempdir().unwrap();
  let fixture_directory = tempfile::tempdir().unwrap();
  let fixture = configured_process_fixture(fixture_directory.path(), "ignore-termination");
  let heartbeat = fixture.with_extension("run-heartbeat");
  let (manager, mut execution) = start_lifecycle_execution(workspace.path(), fixture).await;
  wait_for_file(&heartbeat).await;

  let started = std::time::Instant::now();
  tokio::time::timeout(Duration::from_secs(5), execution.cancel_and_wait())
    .await
    .expect("forced cancellation did not complete")
    .unwrap();
  assert!(
    started.elapsed() >= Duration::from_millis(1500),
    "uncooperative fixture did not exercise the graceful timeout"
  );
  let response = receive_terminal(&mut execution).await;
  assert!(matches!(response, PluginResponse::Completed { code: -1, .. }));
  assert_heartbeat_stopped(&heartbeat).await;
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn plugin_shutdown_cancels_the_active_command_and_stops_its_tree() {
  let workspace = tempfile::tempdir().unwrap();
  let fixture_directory = tempfile::tempdir().unwrap();
  let fixture = configured_process_fixture(fixture_directory.path(), "hang");
  let heartbeat = fixture.with_extension("run-heartbeat");
  let (manager, _execution) = start_lifecycle_execution(workspace.path(), fixture).await;
  wait_for_file(&heartbeat).await;

  let results = tokio::time::timeout(Duration::from_secs(5), manager.shutdown_all())
    .await
    .expect("plugin shutdown did not complete");
  assert!(results.into_iter().all(|result| result.is_ok()));
  assert_heartbeat_stopped(&heartbeat).await;
}

#[tokio::test]
async fn plugin_host_streams_an_auditable_completion_and_stops_its_runtime_descendant() {
  let workspace = tempfile::tempdir().unwrap();
  let fixture_directory = tempfile::tempdir().unwrap();
  let fixture = configured_process_fixture(fixture_directory.path(), "descendant");
  let heartbeat = fixture.with_extension("run-descendant-heartbeat");
  let release = fixture.with_extension("release-run");
  let (manager, mut execution) = start_lifecycle_execution(workspace.path(), fixture).await;
  wait_for_file(&heartbeat).await;

  let first = tokio::time::timeout(
    Duration::from_secs(5),
    execution.receive_output(&CancellationToken::new()),
  )
  .await
  .expect("fixture progress was not observed")
  .unwrap()
  .expect("plugin response stream closed before fixture progress");
  fs::write(release, b"release").unwrap();
  let mut responses = vec![first];
  responses.extend(receive_through_terminal(&mut execution).await);
  let [PluginResponse::Progress { id, progress }, PluginResponse::Stdout {
    id: first_stdout_id,
    line: first_line,
  }, PluginResponse::Stdout {
    id: second_stdout_id,
    line: second_line,
  }, PluginResponse::RegisterArtifact {
    id: trace_id,
    artifact: trace,
  }, PluginResponse::RegisterArtifact {
    id: provenance_id,
    artifact: provenance,
  }, PluginResponse::RegisterReport { id: report_id, report }, PluginResponse::Completed {
    id: terminal_id,
    code: 0,
    outputs,
  }] = responses.as_slice()
  else {
    panic!("unexpected ordered plugin-host responses: {responses:?}");
  };

  assert_eq!(progress.message, "Codex turn started");
  assert_eq!(first_line, "working-1");
  assert_eq!(second_line, "working-2");
  for response_id in [
    first_stdout_id,
    second_stdout_id,
    trace_id,
    provenance_id,
    report_id,
    terminal_id,
  ] {
    assert_eq!(response_id, id, "one invocation must retain one response id");
  }
  assert_eq!(outputs["outcome"], "completed");
  assert_eq!(outputs["final_message"], "done");
  assert_eq!(
    outputs["harness_identifiers"],
    json!({ "thread_id": "fixture-thread", "turn_id": "fixture-turn" })
  );
  assert_eq!(outputs["usage"], json!({ "input_tokens": 3, "output_tokens": 5 }));

  let trace_path = outputs["record_paths"]["trace"].as_str().unwrap();
  let result_path = outputs["record_paths"]["result"].as_str().unwrap();
  let provenance_path = outputs["record_paths"]["provenance"].as_str().unwrap();
  assert_eq!(trace.name, "codex-run-trace");
  assert_eq!(trace.path, Path::new(trace_path));
  assert_eq!(trace.content_type.as_deref(), Some("application/x-ndjson"));
  assert_eq!(provenance.name, "codex-run-provenance");
  assert_eq!(provenance.path, Path::new(provenance_path));
  assert_eq!(provenance.content_type.as_deref(), Some("application/json"));
  assert_eq!(report.name, "codex-run-result");
  assert_eq!(report.path, Path::new(result_path));
  assert_eq!(report.format, "octa.codex.result.v1");

  for name in ["trace", "result", "provenance"] {
    let path = outputs["record_paths"][name].as_str().unwrap();
    assert!(workspace.path().join(path).is_file(), "missing {name} run record");
  }
  let result: Value = serde_json::from_slice(&fs::read(workspace.path().join(result_path)).unwrap()).unwrap();
  assert_eq!(result["format_version"], 1);
  assert_eq!(
    responses
      .iter()
      .filter(|response| matches!(
        response,
        PluginResponse::Completed { .. } | PluginResponse::Error { .. }
      ))
      .count(),
    1
  );
  assert_response_stream_closed(&mut execution).await;
  assert_heartbeat_stopped(&heartbeat).await;
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn semantic_failure_remains_auditable_when_the_harness_exits_nonzero() {
  let workspace = tempfile::tempdir().unwrap();
  let fixture_directory = tempfile::tempdir().unwrap();
  let fixture = configured_process_fixture(fixture_directory.path(), "failed-nonzero");
  let (manager, mut execution) = start_lifecycle_execution(workspace.path(), fixture).await;

  let responses = receive_through_terminal(&mut execution).await;
  let Some(PluginResponse::Completed { code, outputs, .. }) = responses.last() else {
    panic!("semantic failure was not retained as a completion: {responses:?}");
  };
  assert_eq!(*code, 0);
  assert_eq!(outputs["outcome"], "failed");
  assert_eq!(outputs["final_message"], "fixture failure");
  assert_eq!(
    responses
      .iter()
      .filter(|response| matches!(
        response,
        PluginResponse::RegisterArtifact { .. } | PluginResponse::RegisterReport { .. }
      ))
      .count(),
    3,
    "an auditable semantic failure must retain all run records"
  );
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn a_missing_required_deliverable_publishes_no_resources() {
  let workspace = tempfile::tempdir().unwrap();
  let fixture_directory = tempfile::tempdir().unwrap();
  let fixture = configured_process_fixture(fixture_directory.path(), "complete");
  let (manager, plugin_name) = plugin_manager_with_codex(workspace.path(), fixture);
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("codex").await.unwrap();
  let mut request = lifecycle_request(workspace.path());
  request.params = json!({
    "prompt": "perform the fixture task",
    "deliverables": [{
      "kind": "artifact",
      "name": "required-output",
      "path": "missing/output"
    }]
  });
  let mut execution = client.start_execution(request, CancellationToken::new()).await.unwrap();
  let responses = receive_through_terminal(&mut execution).await;

  assert!(matches!(
    responses.last(),
    Some(PluginResponse::Error { message, .. }) if message.contains("missing or inaccessible")
  ));
  assert!(responses.iter().all(|response| !matches!(
    response,
    PluginResponse::RegisterArtifact { .. } | PluginResponse::RegisterReport { .. }
  )));
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}
