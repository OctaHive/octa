//! Deterministic Codex process fixture used by machine-boundary tests.
//!
//! `--version` emits a sibling-file-controlled response without network or
//! credentials. A sibling `run-mode` file selects production-boundary
//! scenarios; direct contract tests use explicit environment overrides so they
//! can execute Cargo's immutable binary without a copy-then-exec race. Marker
//! files coordinate races without sleeps, and every wait remains bounded.

use std::{
  io::{Read, Write},
  path::{Path, PathBuf},
  process::{Command, Stdio},
  thread,
  time::Duration,
};

const SECRET_ENV: &str = "OCTA_CODEX_FIXTURE_SECRET";
const MODE_ENV: &str = "OCTA_CODEX_FIXTURE_MODE";
const CONTROL_DIRECTORY_ENV: &str = "OCTA_CODEX_FIXTURE_CONTROL_DIRECTORY";
const OVERSIZED_FRAME_PAYLOAD_BYTES: usize = 1024 * 1024 + 1;
const CONTROL_WAIT_STEPS: usize = 600;
const CONTROL_WAIT_INTERVAL: Duration = Duration::from_millis(5);
// Exceeds the plugin client's private response queue and the runner's output
// pipe while remaining comfortably below the Codex trace byte limit.
const NOISY_EVENT_COUNT: usize = 10_000;
const TRACE_OVERFLOW_EVENT_COUNT: usize = 20;
// Twenty frames at this size exceed the 16 MiB retained-trace limit while
// each individual frame remains below the 1 MiB protocol-frame limit.
const TRACE_OVERFLOW_PAYLOAD_BYTES: usize = 900 * 1024;
const SECRET_SPILL_EVENT_COUNT: usize = 80;
// The aggregate sanitized output exceeds both 1 MiB disk-spill thresholds but
// remains below the independent trace and runner-capture limits.
const SECRET_SPILL_PADDING_BYTES: usize = 16 * 1024;

fn main() {
  let executable = std::env::current_exe().expect("fixture executable path must be available");
  let control_directory = std::env::var_os(CONTROL_DIRECTORY_ENV).map(PathBuf::from);
  let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
  if arguments
    .first()
    .is_some_and(|argument| argument == "--fixture-descendant")
  {
    let heartbeat = arguments.get(1).expect("fixture descendant requires a heartbeat path");
    run_heartbeat(Path::new(heartbeat));
  }
  if arguments == ["--version"] {
    let response_path = executable.with_extension("version");
    let response = std::fs::read_to_string(&response_path).unwrap_or_else(|_| "codex-cli 0.130.0\n".to_owned());
    std::fs::write(executable.with_extension("version-probed"), b"probed")
      .expect("fixture probe marker must be writable");
    if executable.with_extension("spawn-version-descendant").exists() {
      let heartbeat = executable.with_extension("version-descendant-heartbeat");
      spawn_descendant(&executable, &heartbeat, "version descendant must start");
      for _ in 0..200 {
        if heartbeat.exists() {
          break;
        }
        thread::sleep(Duration::from_millis(5));
      }
      assert!(heartbeat.exists(), "version descendant did not become ready");
    }
    print!("{response}");
    std::io::stdout().flush().expect("version response must flush");
    if executable.with_extension("hang-version").exists() {
      thread::sleep(Duration::from_secs(60));
    }
    return;
  }
  let marker = runtime_path(&executable, control_directory.as_deref(), "spawned");
  std::fs::write(marker, b"spawned").expect("fixture marker must be writable");
  let mut prompt = String::new();
  std::io::stdin()
    .read_to_string(&mut prompt)
    .expect("fixture prompt must be readable");
  assert!(!prompt.is_empty(), "fixture requires a stdin prompt");

  let mode = std::env::var(MODE_ENV).unwrap_or_else(|_| {
    std::fs::read_to_string(executable.with_extension("run-mode")).unwrap_or_else(|_| "complete".to_owned())
  });
  match mode.trim() {
    "complete" => emit_event(r#"{"type":"turn.completed","message":"done"}"#),
    "failed" => emit_event(r#"{"type":"turn.failed","error":{"message":"fixture failure"}}"#),
    "failed-nonzero" => {
      emit_event(r#"{"type":"turn.failed","error":{"message":"fixture failure"}}"#);
      std::process::exit(17);
    },
    "structured" => emit_event(
      r#"{"type":"turn.completed","result":{"outcome":"completed","files":2},"usage":{"input_tokens":3,"output_tokens":5}}"#,
    ),
    "example-local" => {
      write_workspace_file("out/review.md", b"# Fixture review\n\nNo issues found.\n");
      emit_event(r#"{"type":"turn.completed","message":"review completed"}"#);
    },
    "example-agent" => {
      write_workspace_file("out/change.patch", b"fixture patch\n");
      write_workspace_file("out/summary.json", b"{\"status\":\"completed\"}\n");
      emit_event(
        r#"{"type":"turn.completed","result":{"outcome":"completed","files":2},"usage":{"input_tokens":3,"output_tokens":5}}"#,
      );
    },
    "unknown" => {
      emit_event(r#"{"type":"future.additive","payload":{"retained":true}}"#);
      emit_event(r#"{"type":"turn.completed","message":"done"}"#);
    },
    "noisy" => {
      spawn_runtime_descendant(&executable, control_directory.as_deref());
      emit_noisy_stream();
    },
    "noisy-stream" => emit_noisy_stream(),
    "trace-overflow" => {
      spawn_runtime_descendant(&executable, control_directory.as_deref());
      emit_trace_overflow_stream();
    },
    "trace-overflow-stream" => emit_trace_overflow_stream(),
    "partial" => emit_controlled_partial_frame(&executable, control_directory.as_deref()),
    "secret-echo" => emit_secret_echo(),
    "secret-spill" => emit_secret_spill(),
    "malformed" => emit_event("{not-json}"),
    "oversized" => emit_oversized_frame(),
    "duplicate-terminal" => {
      emit_event(r#"{"type":"turn.completed","message":"first"}"#);
      emit_event(r#"{"type":"turn.completed","message":"second"}"#);
    },
    "missing-terminal" => emit_event(r#"{"type":"turn.started"}"#),
    "descendant" => {
      spawn_runtime_descendant(&executable, control_directory.as_deref());
      emit_event(r#"{"type":"turn.started"}"#);
      wait_for_file(
        &runtime_path(&executable, control_directory.as_deref(), "release-run"),
        "auditable descendant fixture was not released",
      );
      emit_event(r#"{"type":"item.completed","item":{"type":"agent_message","text":"working-1"}}"#);
      emit_event(r#"{"type":"item.completed","item":{"type":"agent_message","text":"working-2"}}"#);
      emit_event(
        r#"{"type":"turn.completed","message":"done","thread_id":"fixture-thread","turn_id":"fixture-turn","usage":{"input_tokens":3,"output_tokens":5}}"#,
      );
    },
    "hang" => {
      emit_event(r#"{"type":"turn.started"}"#);
      run_heartbeat(&runtime_path(
        &executable,
        control_directory.as_deref(),
        "run-heartbeat",
      ));
    },
    "ignore-termination" => {
      ignore_cooperative_termination();
      emit_event(r#"{"type":"turn.started"}"#);
      run_heartbeat(&runtime_path(
        &executable,
        control_directory.as_deref(),
        "run-heartbeat",
      ));
    },
    "exit-race" => {
      emit_event(r#"{"type":"turn.started"}"#);
      let ready = runtime_path(&executable, control_directory.as_deref(), "run-heartbeat");
      std::fs::write(&ready, b"ready").expect("race-ready marker must be writable");
      let release = runtime_path(&executable, control_directory.as_deref(), "release-run");
      wait_for_file(&release, "race fixture was not released");
      emit_event(r#"{"type":"turn.completed","message":"done"}"#);
    },
    _ => panic!("unknown Codex fixture run mode"),
  }
}

fn emit_event(event: &str) {
  println!("{event}");
  std::io::stdout().flush().expect("fixture event must flush");
}

fn write_workspace_file(path: &str, contents: &[u8]) {
  let path = Path::new(path);
  std::fs::create_dir_all(path.parent().expect("fixture output path has a parent"))
    .expect("fixture output directory must be writable");
  std::fs::write(path, contents).expect("fixture output file must be writable");
}

fn emit_controlled_partial_frame(executable: &Path, control_directory: Option<&Path>) {
  const EVENT: &str = r#"{"type":"item.completed","item":{"type":"agent_message","text":"partial 🧪"}}"#;
  let unicode = EVENT.find('\u{1f9ea}').expect("partial event contains Unicode");
  let split = unicode + 2;
  let mut stdout = std::io::stdout().lock();
  stdout
    .write_all(&EVENT.as_bytes()[..split])
    .expect("partial event prefix must be writable");
  stdout.flush().expect("partial event prefix must flush");
  let ready = runtime_path(executable, control_directory, "partial-ready");
  std::fs::write(&ready, b"ready").expect("partial-ready marker must be writable");
  wait_for_file(
    &runtime_path(executable, control_directory, "release-run"),
    "partial fixture was not released",
  );
  stdout
    .write_all(&EVENT.as_bytes()[split..])
    .expect("partial event suffix must be writable");
  stdout.write_all(b"\n").expect("partial event newline must be writable");
  stdout
    .write_all(b"{\"type\":\"turn.completed\",\"message\":\"done\"}\n")
    .expect("partial terminal event must be writable");
  stdout.flush().expect("completed partial stream must flush");
}

fn runtime_path(executable: &Path, control_directory: Option<&Path>, name: &str) -> PathBuf {
  control_directory
    .map(|directory| directory.join(name))
    .unwrap_or_else(|| executable.with_extension(name))
}

fn spawn_runtime_descendant(executable: &Path, control_directory: Option<&Path>) {
  let heartbeat = runtime_path(executable, control_directory, "run-descendant-heartbeat");
  spawn_descendant(executable, &heartbeat, "runtime descendant must start");
  wait_for_file(&heartbeat, "runtime descendant did not become ready");
}

fn emit_secret_echo() {
  let secret = std::env::var(SECRET_ENV).expect("secret-echo mode requires its explicit fixture variable");
  let event = serde_json::json!({
    "type": "item.completed",
    "item": { "type": "agent_message", "text": format!("stdout {secret}") }
  });
  emit_event(&serde_json::to_string(&event).expect("fixture event must serialize"));
  eprintln!("stderr {secret}");
  emit_event(r#"{"type":"turn.completed","message":"done"}"#);
}

fn emit_secret_spill() {
  let secret = std::env::var(SECRET_ENV).expect("secret-spill mode requires its explicit fixture variable");
  let padding = "x".repeat(SECRET_SPILL_PADDING_BYTES);
  for index in 0..SECRET_SPILL_EVENT_COUNT {
    let event = serde_json::json!({
      "type": "item.completed",
      "item": {
        "type": "agent_message",
        "text": format!("event-{index} {secret} {padding}")
      }
    });
    emit_event(&serde_json::to_string(&event).expect("fixture event must serialize"));
  }
  eprintln!("stderr {secret}");
  emit_event(r#"{"type":"turn.completed","message":"done"}"#);
}

fn emit_noisy_stream() {
  for index in 0..NOISY_EVENT_COUNT {
    emit_event(&format!(
      r#"{{"type":"item.completed","item":{{"type":"agent_message","text":"event-{index}"}}}}"#
    ));
  }
  emit_event(r#"{"type":"turn.completed","message":"done"}"#);
}

fn emit_trace_overflow_stream() {
  let event = format!(
    r#"{{"type":"future.additive","padding":"{}"}}"#,
    "x".repeat(TRACE_OVERFLOW_PAYLOAD_BYTES)
  );
  for _ in 0..TRACE_OVERFLOW_EVENT_COUNT {
    emit_event(&event);
  }
  emit_event(r#"{"type":"turn.completed","message":"done"}"#);
}

fn emit_oversized_frame() {
  let mut stdout = std::io::stdout().lock();
  stdout
    .write_all(b"{\"type\":\"future.additive\",\"padding\":\"")
    .expect("oversized event prefix must be writable");
  stdout
    .write_all(&vec![b'x'; OVERSIZED_FRAME_PAYLOAD_BYTES])
    .expect("oversized event body must be writable");
  stdout
    .write_all(b"\"}\n")
    .expect("oversized event suffix must be writable");
  stdout.flush().expect("oversized event must flush");
}

fn wait_for_file(path: &Path, failure: &str) {
  for _ in 0..CONTROL_WAIT_STEPS {
    if path.exists() {
      return;
    }
    thread::sleep(CONTROL_WAIT_INTERVAL);
  }
  panic!("{failure}");
}

#[cfg(unix)]
fn ignore_cooperative_termination() {
  // SAFETY: this test fixture deliberately ignores SIGTERM so the plugin's
  // bounded grace period must advance to its force-kill path.
  unsafe {
    libc::signal(libc::SIGTERM, libc::SIG_IGN);
  }
}

#[cfg(not(unix))]
fn ignore_cooperative_termination() {
  // Closing stdin is the portable graceful request on Windows. The fixture
  // has already consumed it and intentionally keeps running.
}

fn run_heartbeat(path: &Path) -> ! {
  let mut counter = 0_u64;
  loop {
    std::fs::write(path, counter.to_string()).expect("heartbeat must be writable");
    thread::sleep(Duration::from_millis(10));
    counter = counter.wrapping_add(1);
  }
}

#[expect(
  clippy::zombie_processes,
  reason = "the fixture intentionally leaves this descendant for the plugin process owner to reap"
)]
fn spawn_descendant(executable: &Path, heartbeat: &Path, failure: &str) {
  Command::new(executable)
    .arg("--fixture-descendant")
    .arg(heartbeat)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap_or_else(|error| panic!("{failure}: {error}"));
}
