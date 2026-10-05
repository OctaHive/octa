//! Minimal Codex process fixture used by invocation contract tests.
//!
//! `--version` emits a sibling-file-controlled response without network or
//! credentials. Other invocations leave an observable marker so dry-run tests
//! can prove the harness was never started. Later lifecycle tasks extend this
//! fixture with JSONL and shutdown scenarios.

use std::{
  io::Write,
  path::Path,
  process::{Command, Stdio},
  thread,
  time::Duration,
};

fn main() {
  let executable = std::env::current_exe().expect("fixture executable path must be available");
  let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
  if arguments
    .first()
    .is_some_and(|argument| argument == "--probe-descendant")
  {
    let heartbeat = arguments.get(1).expect("probe descendant requires a heartbeat path");
    run_heartbeat(Path::new(heartbeat));
  }
  if arguments == ["--version"] {
    let response_path = executable.with_extension("version");
    let response = std::fs::read_to_string(&response_path).unwrap_or_else(|_| "codex-cli 0.130.0\n".to_owned());
    std::fs::write(executable.with_extension("version-probed"), b"probed")
      .expect("fixture probe marker must be writable");
    if executable.with_extension("spawn-version-descendant").exists() {
      let heartbeat = executable.with_extension("version-descendant-heartbeat");
      spawn_probe_descendant(&executable, &heartbeat);
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
  let marker = executable
    .parent()
    .expect("fixture executable must have a parent")
    .join("spawned");
  std::fs::write(marker, b"spawned").expect("fixture marker must be writable");
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
fn spawn_probe_descendant(executable: &Path, heartbeat: &Path) {
  Command::new(executable)
    .arg("--probe-descendant")
    .arg(heartbeat)
    .stdin(Stdio::null())
    .spawn()
    .expect("version descendant must start");
}
