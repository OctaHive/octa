use std::{
  env, fs,
  io::{BufRead, BufReader, Read, Write},
  ops::{Deref, DerefMut},
  path::PathBuf,
  process::{Child, Stdio},
  sync::mpsc,
  thread,
  time::{Duration, Instant},
};

use assert_cmd::Command;
use octa_cache_protocol::{Digest, PlatformArchitecture, PlatformOs, RuntimeIdentity};
use octa_plugin::protocol::PLUGIN_PROTOCOL_VERSION;
use octa_plugin_manager::plugin_lock::{
  current_platform, sha256_file, write_plugin_lock, PluginLock, PluginManifest, PLUGIN_LOCK_VERSION,
  PLUGIN_MANIFEST_VERSION,
};
use octa_runner::{MAX_RUNNER_INPUT_FRAME_BYTES, RUNNER_OUTPUT_SCHEMA_V2, RUNNER_PROTOCOL_VERSION};
use serde_json::{json, Value};
use tempfile::TempDir;
use wait_timeout::ChildExt;

const PROCESS_WATCHDOG: Duration = Duration::from_secs(60);

/// Ensures a failed integration test cannot leave a runner competing with the
/// rest of the concurrently executing suite for CPU, pipes, or plugin sockets.
struct ReapedChild(Child);

impl From<Child> for ReapedChild {
  fn from(child: Child) -> Self {
    Self(child)
  }
}

impl Deref for ReapedChild {
  type Target = Child;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl DerefMut for ReapedChild {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.0
  }
}

impl Drop for ReapedChild {
  fn drop(&mut self) {
    if !matches!(self.0.try_wait(), Ok(Some(_))) {
      let _ = self.0.kill();
      let _ = self.0.wait();
    }
  }
}

fn plugins_dir() -> PathBuf {
  if let Some(path) = env::var_os("OCTA_E2E_PLUGINS_DIR") {
    return PathBuf::from(path);
  }

  let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
  let target = workspace.join("target/debug");
  #[cfg(windows)]
  let plugin_names = ["octa_plugin_shell.exe", "octa_plugin_tpl.exe"];
  #[cfg(not(windows))]
  let plugin_names = ["octa_plugin_shell", "octa_plugin_tpl"];
  if plugin_names.iter().all(|name| target.join(name).is_file()) {
    target
  } else {
    workspace.join("plugins")
  }
}

fn request(workspace: &TempDir, command: &str) -> String {
  format!(
    "{}\n",
    json!({
      "type": "start",
      "protocol_version": RUNNER_PROTOCOL_VERSION,
      "request_id": "test-run",
      "request": {
        "workspace": workspace.path(),
        "data_dir": workspace.path().join("cache"),
        "plugins_dir": plugins_dir(),
        "commands": [command]
      }
    })
  )
}

fn cached_request(workspace: &TempDir, cache_directory: &std::path::Path, command: &str) -> String {
  let os = match env::consts::OS {
    "linux" => PlatformOs::Linux,
    "windows" => PlatformOs::Windows,
    "macos" => PlatformOs::Macos,
    value => panic!("unsupported test operating system {value}"),
  };
  let architecture = match env::consts::ARCH {
    "x86_64" => PlatformArchitecture::Amd64,
    "aarch64" => PlatformArchitecture::Arm64,
    value => panic!("unsupported test architecture {value}"),
  };
  let runtime = RuntimeIdentity::Native {
    os,
    architecture,
    environment: Digest::blake3(b"cli-runner-shared-test-environment"),
  };
  format!(
    "{}\n",
    json!({
      "type": "start",
      "protocol_version": 2,
      "request_id": "cached-run",
      "request": {
        "workspace": workspace.path(),
        "data_dir": workspace.path().join("state"),
        "plugins_dir": plugins_dir(),
        "commands": [command],
        "cache": {
          "mode": "read_write",
          "namespace": "tests/runner",
          "local_directory": cache_directory,
          "runtime": runtime
        }
      }
    })
  )
}

fn locked_plugins(workspace: &TempDir) -> (PathBuf, PathBuf) {
  let directory = workspace.path().join("locked-plugins");
  fs::create_dir(&directory).unwrap();
  let mut manifests = Vec::new();
  for name in ["shell", "tpl"] {
    #[cfg(windows)]
    let entrypoint = format!("octa_plugin_{name}.exe");
    #[cfg(not(windows))]
    let entrypoint = format!("octa_plugin_{name}");
    let source = plugins_dir().join(&entrypoint);
    let destination = directory.join(&entrypoint);
    fs::copy(source, &destination).unwrap();
    let digest = tokio::runtime::Runtime::new()
      .unwrap()
      .block_on(sha256_file(&destination))
      .unwrap();
    let manifest = PluginManifest {
      manifest_version: PLUGIN_MANIFEST_VERSION,
      name: name.to_owned(),
      version: env!("CARGO_PKG_VERSION").to_owned(),
      protocol: PLUGIN_PROTOCOL_VERSION,
      platforms: vec![current_platform()],
      entrypoint: entrypoint.into(),
      sha256: digest,
      capabilities: if name == "shell" {
        vec!["shell".to_owned()]
      } else {
        Vec::new()
      },
    };
    let manifest_path = directory.join(format!("{name}.plugin.yml"));
    fs::write(&manifest_path, serde_yml::to_string(&manifest).unwrap()).unwrap();
    manifests.push(manifest);
  }
  let lock = PluginLock {
    version: PLUGIN_LOCK_VERSION,
    plugins: manifests
      .into_iter()
      .map(|manifest| {
        let name = manifest.name.clone();
        (name.clone(), manifest.into_locked(format!("{name}.plugin.yml")))
      })
      .collect(),
  };
  let lock_path = workspace.path().join("Octa.lock");
  write_plugin_lock(&lock, &lock_path).unwrap();
  (directory, lock_path)
}

fn messages(output: &[u8]) -> Vec<Value> {
  std::str::from_utf8(output)
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect()
}

fn assert_valid_output(messages: &[Value]) {
  let schema = serde_json::from_str(RUNNER_OUTPUT_SCHEMA_V2).unwrap();
  let validator = jsonschema::validator_for(&schema).unwrap();
  for message in messages {
    let errors = validator
      .iter_errors(message)
      .map(|error| error.to_string())
      .collect::<Vec<_>>();
    assert!(errors.is_empty(), "invalid runner message {message}: {errors:?}");
  }
}

#[test]
fn reports_capabilities_without_starting_a_job() {
  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.arg("capabilities").output().unwrap();
  assert!(output.status.success());

  let messages = messages(&output.stdout);
  assert_eq!(messages.len(), 1);
  assert_valid_output(&messages);
  assert_eq!(messages[0]["type"], "capabilities");
  assert_eq!(messages[0]["runner_protocols"], json!([2]));
  assert_eq!(messages[0]["event_schemas"], json!([4]));
  assert_eq!(messages[0]["plugin_protocols"], json!([1]));
  assert_eq!(messages[0]["octafile_versions"], json!([1]));
}

#[test]
fn protocol_v2_reuses_one_local_result_across_workspaces() {
  let cache = TempDir::new().unwrap();
  let first = TempDir::new().unwrap();
  let second = TempDir::new().unwrap();
  let octafile = r#"
version: 1
tasks:
  build:
    files:
      inputs: [input.txt]
      outputs: [output.txt]
    cache: {}
    shell: echo generated > output.txt && echo run >> runs.txt
"#;
  for workspace in [&first, &second] {
    fs::write(workspace.path().join("Octafile.yml"), octafile).unwrap();
    fs::write(workspace.path().join("input.txt"), "input").unwrap();
  }

  let execute = |workspace: &TempDir| {
    let mut command = Command::cargo_bin("octa-runner").unwrap();
    command
      .write_stdin(cached_request(workspace, cache.path(), "build"))
      .output()
      .unwrap()
  };
  let first_output = execute(&first);
  assert!(
    first_output.status.success(),
    "{}",
    String::from_utf8_lossy(&first_output.stdout)
  );
  let second_output = execute(&second);
  assert!(
    second_output.status.success(),
    "{}",
    String::from_utf8_lossy(&second_output.stdout)
  );
  assert_valid_output(&messages(&first_output.stdout));
  assert_valid_output(&messages(&second_output.stdout));

  let cache_outcome = |output: &[u8]| {
    messages(output)
      .into_iter()
      .find(|message| message["type"] == "finished")
      .unwrap()["results"][0]["tasks"]
      .as_array()
      .unwrap()
      .iter()
      .find_map(|task| task.get("cache").cloned())
      .unwrap()
  };
  let first_cache = cache_outcome(&first_output.stdout);
  let second_cache = cache_outcome(&second_output.stdout);
  assert_eq!(first_cache["status"], "miss");
  assert_eq!(
    second_cache["status"], "hit",
    "first={first_cache}, second={second_cache}"
  );
  assert_eq!(first_cache["action"], second_cache["action"]);
  assert_eq!(
    fs::read_to_string(second.path().join("output.txt")).unwrap(),
    "generated\n"
  );
  assert!(!second.path().join("runs.txt").exists());
}

#[test]
fn executes_only_digest_verified_locked_plugins() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  build:\n    shell: echo locked\n",
  )
  .unwrap();
  let (plugins_dir, lock_path) = locked_plugins(&workspace);
  let input = format!(
    "{}\n",
    json!({
      "type": "start",
      "protocol_version": RUNNER_PROTOCOL_VERSION,
      "request_id": "locked-run",
      "request": {
        "workspace": workspace.path(),
        "data_dir": workspace.path().join("cache"),
        "plugins_dir": plugins_dir,
        "plugin_lock": lock_path,
        "commands": ["build"]
      }
    })
  );

  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin(input.clone()).output().unwrap();
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stdout));
  assert_eq!(messages(&output.stdout).last().unwrap()["status"], "succeeded");

  #[cfg(windows)]
  let shell_entrypoint = "octa_plugin_shell.exe";
  #[cfg(not(windows))]
  let shell_entrypoint = "octa_plugin_shell";
  fs::write(
    workspace.path().join("locked-plugins").join(shell_entrypoint),
    "tampered",
  )
  .unwrap();
  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let rejected = command.write_stdin(input).output().unwrap();
  assert_eq!(rejected.status.code(), Some(2));
  assert!(messages(&rejected.stdout).last().unwrap()["message"]
    .as_str()
    .unwrap()
    .contains("digest mismatch"));
}

#[test]
fn streams_events_and_finishes_with_a_structured_result() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  build:\n    shell: echo runner-output\n",
  )
  .unwrap();

  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin(request(&workspace, "build")).output().unwrap();
  assert!(
    output.status.success(),
    "stdout: {}\nstderr: {}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let messages = messages(&output.stdout);
  assert_valid_output(&messages);
  assert_eq!(messages.first().unwrap()["type"], "hello");
  assert_eq!(messages[1]["type"], "accepted");
  assert!(messages
    .iter()
    .any(|message| { message["type"] == "event" && message["event"]["data"]["type"] == "run_finished" }));
  let finished = messages.last().unwrap();
  assert_eq!(finished["type"], "finished");
  assert_eq!(finished["request_id"], "test-run");
  assert_eq!(finished["status"], "succeeded");
  assert_eq!(finished["results"][0]["conclusion"]["status"], "succeeded");
  assert_eq!(finished["results"][0]["stdout"], json!(["runner-output"]));

  let sequences = messages
    .iter()
    .filter(|message| message["type"] == "event")
    .map(|message| message["event"]["sequence"].as_u64().unwrap())
    .collect::<Vec<_>>();
  assert!(!sequences.is_empty());
  assert!(sequences.windows(2).all(|pair| pair[1] == pair[0] + 1));
}

#[test]
fn returns_validated_artifacts_and_reports() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    r#"version: 1
tasks:
  build:
    shell: mkdir -p dist reports && printf binary > dist/app && printf '<testsuite/>' > reports/junit.xml
    artifacts:
      - name: application
        path: dist
        content_type: application/octet-stream
    reports:
      - name: tests
        path: reports/junit.xml
        format: junit
"#,
  )
  .unwrap();

  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin(request(&workspace, "build")).output().unwrap();
  assert!(
    output.status.success(),
    "stdout: {}\nstderr: {}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let messages = messages(&output.stdout);
  assert_valid_output(&messages);
  assert!(messages
    .iter()
    .any(|message| { message["type"] == "event" && message["event"]["data"]["type"] == "artifact_registered" }));
  assert!(messages
    .iter()
    .any(|message| { message["type"] == "event" && message["event"]["data"]["type"] == "report_registered" }));
  let task = &messages.last().unwrap()["results"][0]["tasks"][0];
  assert_eq!(task["artifacts"][0]["name"], "application");
  assert_eq!(task["artifacts"][0]["path"], "dist");
  assert_eq!(task["reports"][0]["name"], "tests");
  assert_eq!(task["reports"][0]["path"], "reports/junit.xml");
  assert_eq!(task["reports"][0]["format"], "junit");
}

#[test]
fn junit_plugin_registers_a_report_without_core_format_logic() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    r#"version: 1
tasks:
  test:
    cmds:
      - shell: mkdir -p reports && printf '<testsuite/>' > reports/junit.xml
      - junit:
          name: unit-tests
          path: reports/junit.xml
"#,
  )
  .unwrap();
  let input = format!(
    "{}\n",
    json!({
      "type": "start",
      "protocol_version": RUNNER_PROTOCOL_VERSION,
      "request_id": "junit-run",
      "request": {
        "workspace": workspace.path(),
        "data_dir": workspace.path().join("cache"),
        "plugins_dir": plugins_dir(),
        "plugins": ["junit"],
        "commands": ["test"]
      }
    })
  );

  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin(input).output().unwrap();
  assert!(
    output.status.success(),
    "stdout: {}\nstderr: {}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let messages = messages(&output.stdout);
  assert_valid_output(&messages);
  let task = &messages.last().unwrap()["results"][0]["tasks"][0];
  let report = task["steps"]
    .as_array()
    .unwrap()
    .iter()
    .flat_map(|step| step["reports"].as_array().into_iter().flatten())
    .find(|report| report["name"] == "unit-tests")
    .unwrap();
  assert_eq!(report["path"], "reports/junit.xml");
  assert_eq!(report["format"], "junit");
}

#[test]
fn uses_environment_specific_profiles_without_changing_the_octafile() {
  let workspace = TempDir::new().unwrap();
  for (environment, value) in [("local", "local-secret-value"), ("agent", "agent-secret-value")] {
    let store = workspace.path().join(format!("secret-store-{environment}"));
    fs::create_dir(&store).unwrap();
    fs::write(store.join("token"), format!("{value}\n")).unwrap();
    fs::write(
      workspace.path().join(format!("secrets-{environment}.yml")),
      format!("version: 1\nproviders:\n  application:\n    type: file\n    root: secret-store-{environment}\n"),
    )
    .unwrap();
  }
  fs::write(
    workspace.path().join("Octafile.yml"),
    r#"version: 1
vars:
  TOKEN:
    secret:
      provider: application
      key: token
tasks:
  show:
    shell: printf '%s' "{{ TOKEN }}" > observed.txt && echo "{{ TOKEN }}"
"#,
  )
  .unwrap();
  for (environment, value) in [("local", "local-secret-value"), ("agent", "agent-secret-value")] {
    let input = format!(
      "{}\n",
      json!({
        "type": "start",
        "protocol_version": RUNNER_PROTOCOL_VERSION,
        "request_id": format!("secret-{environment}"),
        "request": {
          "workspace": workspace.path(),
          "data_dir": workspace.path().join(format!("cache-{environment}")),
          "plugins_dir": plugins_dir(),
          "secrets_profile": format!("secrets-{environment}.yml"),
          "commands": ["show"]
        }
      })
    );

    let mut command = Command::cargo_bin("octa-runner").unwrap();
    let output = command.write_stdin(input).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let encoded = String::from_utf8(output.stdout).unwrap();
    assert!(!encoded.contains(value), "runner leaked a secret: {encoded}");
    assert_eq!(
      fs::read_to_string(workspace.path().join("observed.txt")).unwrap(),
      value
    );
    let messages = messages(encoded.as_bytes());
    assert_valid_output(&messages);
    assert_eq!(messages.last().unwrap()["results"][0]["stdout"], json!(["*****"]));
  }
}

#[test]
fn task_failure_is_a_finished_execution_with_a_failure_exit_code() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  fail:\n    shell: exit 7\n",
  )
  .unwrap();

  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin(request(&workspace, "fail")).output().unwrap();
  assert_eq!(output.status.code(), Some(1));

  let messages = messages(&output.stdout);
  let finished = messages.last().unwrap();
  assert_eq!(finished["type"], "finished");
  assert_eq!(finished["status"], "failed");
  assert_eq!(finished["results"][0]["conclusion"]["status"], "failed");
  assert_eq!(finished["results"][0]["conclusion"]["failure"]["exit_code"], 7);
}

#[test]
fn missing_task_is_rejected_after_the_request_is_accepted() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  build:\n    shell: echo build\n",
  )
  .unwrap();

  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin(request(&workspace, "missing")).output().unwrap();
  assert_eq!(output.status.code(), Some(2));

  let messages = messages(&output.stdout);
  assert_valid_output(&messages);
  assert_eq!(messages[1]["type"], "accepted");
  assert_eq!(messages.last().unwrap()["type"], "error");
  assert_eq!(messages.last().unwrap()["request_id"], "test-run");
  assert!(messages.last().unwrap()["message"]
    .as_str()
    .unwrap()
    .contains("missing"));
}

#[test]
fn rejects_an_incompatible_protocol_before_loading_the_workspace() {
  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command
    .write_stdin(
      json!({
        "type": "start",
        "protocol_version": 99,
        "request_id": "bad-version",
        "request": {
          "workspace": "/does/not/matter",
          "commands": ["build"]
        }
      })
      .to_string()
        + "\n",
    )
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(4));

  let messages = messages(&output.stdout);
  assert_eq!(messages[0]["type"], "hello");
  assert_eq!(messages[1]["type"], "error");
  assert!(messages[1]["message"]
    .as_str()
    .unwrap()
    .contains("unsupported runner protocol"));
}

#[test]
fn rejects_the_removed_protocol_v1_without_loading_a_job() {
  let workspace = TempDir::new().unwrap();
  let cache = TempDir::new().unwrap();
  let mut input: Value = serde_json::from_str(cached_request(&workspace, cache.path(), "build").trim()).unwrap();
  input["protocol_version"] = json!(1);

  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin(input.to_string() + "\n").output().unwrap();
  assert_eq!(output.status.code(), Some(4));

  let messages = messages(&output.stdout);
  assert_valid_output(&messages);
  assert_eq!(messages.last().unwrap()["type"], "error");
  assert!(messages.last().unwrap()["message"]
    .as_str()
    .unwrap()
    .contains("unsupported runner protocol"));
}

#[test]
fn rejects_an_oversized_start_frame() {
  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command
    .write_stdin(vec![b'x'; MAX_RUNNER_INPUT_FRAME_BYTES + 1])
    .output()
    .unwrap();
  assert_eq!(output.status.code(), Some(4));

  let messages = messages(&output.stdout);
  assert_eq!(messages[0]["type"], "hello");
  assert_eq!(messages[1]["type"], "error");
  assert!(messages[1]["message"].as_str().unwrap().contains("frame limit"));
}

#[test]
fn rejects_malformed_json_without_loading_a_workspace() {
  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin("{not-json}\n").output().unwrap();
  assert_eq!(output.status.code(), Some(4));
  let messages = messages(&output.stdout);
  assert_eq!(messages[0]["type"], "hello");
  assert_eq!(messages[1]["type"], "error");
  assert_eq!(messages[1]["request_id"], Value::Null);
}

#[test]
fn rejects_missing_start_cancel_first_empty_ids_and_invalid_requests() {
  let runner = Command::cargo_bin("octa-runner").unwrap();
  let output = std::process::Command::new(runner.get_program()).output().unwrap();
  assert_eq!(output.status.code(), Some(4));
  assert!(messages(&output.stdout).last().unwrap()["message"]
    .as_str()
    .unwrap()
    .contains("expected a start"));

  let cases = [
    (json!({"type": "cancel", "request_id": "test-run"}), 4),
    (
      json!({
        "type": "start",
        "protocol_version": RUNNER_PROTOCOL_VERSION,
        "request_id": "",
        "request": {"workspace": "/", "commands": ["build"]}
      }),
      2,
    ),
    (
      json!({
        "type": "start",
        "protocol_version": RUNNER_PROTOCOL_VERSION,
        "request_id": "invalid",
        "request": {"workspace": "relative", "commands": ["build"]}
      }),
      2,
    ),
  ];
  for (input, exit) in cases {
    let output = std::process::Command::new(runner.get_program())
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .and_then(|mut child| {
        writeln!(child.stdin.take().unwrap(), "{input}")?;
        child.wait_with_output()
      })
      .unwrap();
    assert_eq!(
      output.status.code(),
      Some(exit),
      "{}",
      String::from_utf8_lossy(&output.stdout)
    );
  }
}

#[test]
fn supports_unicode_workspaces_and_partial_output_lines() {
  let root = TempDir::new().unwrap();
  let path = root.path().join("workspace with spaces-данные");
  fs::create_dir(&path).unwrap();
  fs::write(
    path.join("Octafile.yml"),
    "version: 1\ntasks:\n  partial:\n    shell: printf partial-output\n",
  )
  .unwrap();
  let input = format!(
    "{}\n",
    json!({
      "type": "start",
      "protocol_version": RUNNER_PROTOCOL_VERSION,
      "request_id": "unicode",
      "request": {
        "workspace": path,
        "data_dir": root.path().join("cache"),
        "plugins_dir": plugins_dir(),
        "commands": ["partial"]
      }
    })
  );

  let mut command = Command::cargo_bin("octa-runner").unwrap();
  let output = command.write_stdin(input).output().unwrap();
  assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
  let messages = messages(&output.stdout);
  assert_valid_output(&messages);
  assert_eq!(
    messages.last().unwrap()["results"][0]["stdout"],
    json!(["partial-output"])
  );
}

#[test]
fn bounded_event_stream_recovers_after_a_slow_consumer() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  noisy:\n    shell: seq 1 10000\n",
  )
  .unwrap();
  let runner = Command::cargo_bin("octa-runner").unwrap();
  let mut child = ReapedChild::from(
    std::process::Command::new(runner.get_program())
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .unwrap(),
  );
  child
    .stdin
    .take()
    .unwrap()
    .write_all(request(&workspace, "noisy").as_bytes())
    .unwrap();

  thread::sleep(Duration::from_millis(250));
  let mut stdout = child.stdout.take().unwrap();
  let reader = thread::spawn(move || {
    let mut bytes = Vec::new();
    stdout.read_to_end(&mut bytes).unwrap();
    bytes
  });
  let Some(status) = child.wait_timeout(PROCESS_WATCHDOG).unwrap() else {
    panic!("runner did not recover after the consumer resumed reading");
  };
  assert!(status.success());
  let messages = messages(&reader.join().unwrap());
  assert_valid_output(&messages);
  assert_eq!(messages.last().unwrap()["status"], "succeeded");
  let captured = messages.last().unwrap()["results"][0]["stdout"][0].as_str().unwrap();
  assert!(captured.starts_with("1\n2\n"));
  assert!(captured.ends_with("9999\n10000"));
}

#[test]
fn cancel_before_execution_returns_one_terminal_result() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  wait:\n    shell: sleep 30\n",
  )
  .unwrap();

  let runner = Command::cargo_bin("octa-runner").unwrap();
  let mut command = std::process::Command::new(runner.get_program());
  let mut child = ReapedChild::from(command.stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap());
  let mut stdin = child.stdin.take().unwrap();
  let mut stdout = BufReader::new(child.stdout.take().unwrap());
  stdin.write_all(request(&workspace, "wait").as_bytes()).unwrap();
  stdin.flush().unwrap();

  let mut prefix = String::new();
  stdout.read_line(&mut prefix).unwrap();
  stdout.read_line(&mut prefix).unwrap();
  assert!(prefix.contains("\"type\":\"hello\""));
  assert!(prefix.contains("\"type\":\"accepted\""));

  writeln!(stdin, "{}", json!({"type": "cancel", "request_id": "test-run"})).unwrap();
  drop(stdin);

  let Some(status) = child.wait_timeout(PROCESS_WATCHDOG).unwrap() else {
    panic!("runner did not finish after cancellation");
  };
  assert_eq!(status.code(), Some(130));

  let mut tail = String::new();
  stdout.read_to_string(&mut tail).unwrap();
  let finished = tail
    .lines()
    .map(|line| serde_json::from_str::<Value>(line).unwrap())
    .filter(|message| message["type"] == "finished")
    .collect::<Vec<_>>();
  assert_eq!(finished.len(), 1, "runner must emit exactly one terminal result");
  assert_eq!(finished[0]["status"], "cancelled");
  assert_eq!(finished[0]["results"], json!([]));
}

#[test]
fn oversized_control_frame_cancels_the_active_request() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  wait:\n    shell: sleep 30\n",
  )
  .unwrap();
  let runner = Command::cargo_bin("octa-runner").unwrap();
  let mut child = ReapedChild::from(
    std::process::Command::new(runner.get_program())
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .unwrap(),
  );
  let mut stdin = child.stdin.take().unwrap();
  let mut stdout = BufReader::new(child.stdout.take().unwrap());
  stdin.write_all(request(&workspace, "wait").as_bytes()).unwrap();
  stdin.flush().unwrap();
  let mut line = String::new();
  stdout.read_line(&mut line).unwrap();
  line.clear();
  stdout.read_line(&mut line).unwrap();
  assert!(line.contains("\"type\":\"accepted\""));

  stdin.write_all(&vec![b'x'; MAX_RUNNER_INPUT_FRAME_BYTES + 1]).unwrap();
  drop(stdin);
  let Some(status) = child.wait_timeout(PROCESS_WATCHDOG).unwrap() else {
    panic!("runner did not cancel after an oversized control frame");
  };
  assert_eq!(status.code(), Some(130));
  let mut tail = String::new();
  stdout.read_to_string(&mut tail).unwrap();
  assert!(tail.contains("failed to read control command"));
  assert!(tail.contains("\"status\":\"cancelled\""));
}

#[test]
fn cancel_stops_an_active_command() {
  let workspace = TempDir::new().unwrap();
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  wait:\n    shell: sleep 30\n",
  )
  .unwrap();

  let runner = Command::cargo_bin("octa-runner").unwrap();
  let mut child = ReapedChild::from(
    std::process::Command::new(runner.get_program())
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()
      .unwrap(),
  );
  let mut stdin = child.stdin.take().unwrap();
  let stdout = child.stdout.take().unwrap();
  let (line_tx, line_rx) = mpsc::channel();
  let reader = thread::spawn(move || {
    for line in BufReader::new(stdout).lines() {
      if line_tx.send(line).is_err() {
        break;
      }
    }
  });
  stdin.write_all(request(&workspace, "wait").as_bytes()).unwrap();
  stdin.flush().unwrap();

  let mut observed = Vec::new();
  loop {
    let line = line_rx
      .recv_timeout(PROCESS_WATCHDOG)
      .expect("runner did not start the command before the watchdog elapsed")
      .unwrap();
    let message: Value = serde_json::from_str(&line).unwrap();
    let started = message["type"] == "event" && message["event"]["data"]["type"] == "step_started";
    observed.push(message);
    if started {
      break;
    }
  }

  writeln!(stdin, "{}", json!({"type": "cancel", "request_id": "another-run"})).unwrap();
  writeln!(stdin, "{{not-json}}").unwrap();
  stdin.write_all(request(&workspace, "wait").as_bytes()).unwrap();
  stdin.flush().unwrap();
  let deadline = Instant::now() + PROCESS_WATCHDOG;
  let mut errors = 0;
  while errors < 3 {
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
      !remaining.is_zero(),
      "runner did not report all invalid control commands"
    );
    let line = line_rx
      .recv_timeout(remaining)
      .expect("runner did not report the invalid control command")
      .unwrap();
    let message: Value = serde_json::from_str(&line).unwrap();
    errors += usize::from(message["type"] == "error");
    observed.push(message);
  }
  assert!(
    child.try_wait().unwrap().is_none(),
    "invalid control commands cancelled the job"
  );

  writeln!(stdin, "{}", json!({"type": "cancel", "request_id": "test-run"})).unwrap();
  drop(stdin);
  let Some(status) = child.wait_timeout(PROCESS_WATCHDOG).unwrap() else {
    panic!("runner did not stop the active command");
  };
  assert_eq!(status.code(), Some(130));

  reader.join().unwrap();
  observed.extend(
    line_rx
      .try_iter()
      .map(|line| serde_json::from_str::<Value>(&line.unwrap()).unwrap()),
  );
  let terminal = observed
    .iter()
    .filter(|message| message["type"] == "finished")
    .collect::<Vec<_>>();
  assert_eq!(terminal.len(), 1);
  let finished = terminal
    .first()
    .expect("runner must emit a terminal result after cancellation");
  assert_eq!(finished["status"], "cancelled");
  assert_eq!(
    finished["results"][0]["conclusion"]["status"], "cancelled",
    "unexpected result: {finished}"
  );
  assert_valid_output(&observed);
}
