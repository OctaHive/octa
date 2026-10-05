//! Black-box contract tests for the deterministic Codex executable fixture.
//!
//! Each scenario starts from an empty environment and communicates only over
//! stdin/stdout/stderr plus files in a temporary directory. This keeps CI
//! independent of the network, credentials, a shell, and an installed Codex.

use std::{fs, io::Read, io::Write, path::Path, process::Stdio, time::Duration};

use serde_json::Value;

mod support;

use support::configured_process_fixture;

const FIXTURE_SECRET_ENV: &str = "OCTA_CODEX_FIXTURE_SECRET";

struct FixtureOutput {
  stdout: Vec<u8>,
  stderr: Vec<u8>,
}

fn run_fixture(mode: &str, environment: Option<(&str, &str)>) -> FixtureOutput {
  let directory = tempfile::tempdir().unwrap();
  let executable = configured_process_fixture(directory.path(), mode);
  let mut command = std::process::Command::new(executable);
  command
    .env_clear()
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  if let Some((name, value)) = environment {
    command.env(name, value);
  }
  let mut child = command.spawn().unwrap();
  child.stdin.take().unwrap().write_all(b"fixture prompt").unwrap();
  let output = child.wait_with_output().unwrap();
  assert!(output.status.success(), "fixture mode {mode} failed");
  FixtureOutput {
    stdout: output.stdout,
    stderr: output.stderr,
  }
}

fn json_lines(bytes: &[u8]) -> Vec<Value> {
  std::str::from_utf8(bytes)
    .unwrap()
    .lines()
    .map(|line| serde_json::from_str(line).unwrap())
    .collect()
}

#[test]
fn fixture_emits_terminal_structured_unknown_and_secret_scenarios_without_ambient_state() {
  for (mode, terminal) in [("complete", "turn.completed"), ("failed", "turn.failed")] {
    let output = run_fixture(mode, None);
    let events = json_lines(&output.stdout);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], terminal);
    assert!(output.stderr.is_empty());
  }

  let structured = json_lines(&run_fixture("structured", None).stdout);
  assert_eq!(
    structured[0]["result"],
    serde_json::json!({ "outcome": "completed", "files": 2 })
  );
  assert_eq!(
    structured[0]["usage"],
    serde_json::json!({ "input_tokens": 3, "output_tokens": 5 })
  );

  let unknown = json_lines(&run_fixture("unknown", None).stdout);
  assert_eq!(unknown[0]["type"], "future.additive");
  assert_eq!(unknown[1]["type"], "turn.completed");

  let secret = "fixture-secret-sentinel";
  let echoed = run_fixture("secret-echo", Some((FIXTURE_SECRET_ENV, secret)));
  assert!(String::from_utf8(echoed.stdout).unwrap().contains(secret));
  assert!(String::from_utf8(echoed.stderr).unwrap().contains(secret));
}

#[test]
fn fixture_exposes_each_invalid_stream_shape_deterministically() {
  let malformed = run_fixture("malformed", None);
  assert!(serde_json::from_slice::<Value>(&malformed.stdout).is_err());

  let oversized = run_fixture("oversized", None);
  assert!(oversized.stdout.len() > 1024 * 1024);

  let duplicate = json_lines(&run_fixture("duplicate-terminal", None).stdout);
  assert_eq!(duplicate.len(), 2);
  assert!(duplicate.iter().all(|event| event["type"] == "turn.completed"));

  let missing = json_lines(&run_fixture("missing-terminal", None).stdout);
  assert_eq!(missing.len(), 1);
  assert_eq!(missing[0]["type"], "turn.started");
}

#[test]
fn partial_frame_waits_for_an_explicit_release_and_splits_unicode() {
  let directory = tempfile::tempdir().unwrap();
  let executable = configured_process_fixture(directory.path(), "partial");
  let ready = executable.with_extension("partial-ready");
  let release = executable.with_extension("release-run");
  let mut command = std::process::Command::new(&executable);
  command
    .env_clear()
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  let mut child = command.spawn().unwrap();
  let mut stdout = child.stdout.take().unwrap();
  let mut stderr = child.stderr.take().unwrap();
  child.stdin.take().unwrap().write_all(b"fixture prompt").unwrap();

  wait_for_file(&ready);
  assert!(
    child.try_wait().unwrap().is_none(),
    "partial fixture exited before release"
  );
  let mut prefix = vec![0_u8; 4096];
  let prefix_bytes = stdout.read(&mut prefix).unwrap();
  prefix.truncate(prefix_bytes);
  assert!(!prefix.contains(&b'\n'), "fixture completed a frame before release");
  assert!(
    std::str::from_utf8(&prefix).is_err(),
    "fixture did not split the Unicode scalar across writes"
  );

  fs::write(release, b"release").unwrap();
  let mut suffix = Vec::new();
  stdout.read_to_end(&mut suffix).unwrap();
  let mut stderr_bytes = Vec::new();
  stderr.read_to_end(&mut stderr_bytes).unwrap();
  let status = child.wait().unwrap();
  assert!(status.success());
  prefix.extend(suffix);
  let events = json_lines(&prefix);
  assert_eq!(events[0]["item"]["text"], "partial 🧪");
  assert_eq!(events[1]["type"], "turn.completed");
  assert!(stderr_bytes.is_empty());
}

fn wait_for_file(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(5);
  while std::time::Instant::now() < deadline {
    if path.exists() {
      return;
    }
    std::thread::sleep(Duration::from_millis(10));
  }
  panic!("fixture did not create {}", path.display());
}
