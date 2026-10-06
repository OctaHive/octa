use super::*;

use std::{
  fs::OpenOptions,
  io::{Read as _, Write as _},
  path::{Path, PathBuf},
  process::Stdio,
  thread,
};

use tokio::io::AsyncWriteExt as _;

const FIXTURE_MODE_ENV: &str = "OCTA_CODEX_PROCESS_FIXTURE_MODE";
const FIXTURE_HEARTBEAT_ENV: &str = "OCTA_CODEX_PROCESS_FIXTURE_HEARTBEAT";
const FIXTURE_TEST_NAME: &str = "process::tests::owned_process_fixture";

#[tokio::test]
async fn bounded_reader_accepts_the_limit_and_rejects_the_next_byte() {
  let complete = read_bounded([b'x'; 8].as_slice(), 8).await.unwrap();
  assert!(matches!(complete, CapturedStream::Complete(bytes) if bytes.len() == 8));

  let exceeded = read_bounded([b'x'; 9].as_slice(), 8).await.unwrap();
  assert!(matches!(exceeded, CapturedStream::Exceeded));
}

#[tokio::test]
async fn bounded_reader_join_aborts_pipes_that_never_close() {
  let (_stdout_writer, stdout_reader) = tokio::io::duplex(1);
  let (_stderr_writer, stderr_reader) = tokio::io::duplex(1);
  let stdout = tokio::spawn(read_bounded(stdout_reader, 8));
  let stderr = tokio::spawn(read_bounded(stderr_reader, 8));

  let error = match join_readers_bounded(stdout, stderr, Duration::from_millis(20)).await {
    Ok(_) => panic!("open inherited pipes must hit the drain deadline"),
    Err(error) => error,
  };
  assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
}

#[tokio::test]
async fn version_probe_bounds_cancellation_and_timeout() {
  let temporary = tempfile::tempdir().unwrap();
  let cancelled_heartbeat = temporary.path().join("cancelled-heartbeat");
  let cancelled = CancellationToken::new();
  cancelled.cancel();
  let cancelled_outcome = run_version_probe(
    fixture_command("linger", &cancelled_heartbeat),
    Duration::from_secs(1),
    1_024,
    &cancelled,
  )
  .await
  .unwrap();
  assert!(matches!(cancelled_outcome, ProbeOutcome::Cancelled));

  let timed_out_heartbeat = temporary.path().join("timed-out-heartbeat");
  let timed_out = run_version_probe(
    fixture_command("linger", &timed_out_heartbeat),
    Duration::from_millis(20),
    1_024,
    &CancellationToken::new(),
  )
  .await
  .unwrap();
  assert!(matches!(timed_out, ProbeOutcome::TimedOut));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn wait_without_reaping_rejects_an_unknown_child() {
  assert!(process_exited_without_reaping(i32::MAX).is_err());
  assert!(signal_process_group(i32::MAX, libc::SIGKILL).is_ok());
}

#[test]
fn owned_process_fixture() {
  let Some(mode) = std::env::var_os(FIXTURE_MODE_ENV) else {
    return;
  };
  let heartbeat = PathBuf::from(std::env::var_os(FIXTURE_HEARTBEAT_ENV).expect("fixture heartbeat path"));
  if mode == "descendant" {
    let mut file = OpenOptions::new()
      .create(true)
      .append(true)
      .open(heartbeat)
      .expect("open fixture heartbeat");
    loop {
      file.write_all(b"x").expect("write fixture heartbeat");
      file.flush().expect("flush fixture heartbeat");
      thread::sleep(Duration::from_millis(10));
    }
  }

  let mut input = String::new();
  std::io::stdin().read_to_string(&mut input).expect("read fixture stdin");
  println!("fixture stdout: {input}");
  eprintln!("fixture stderr: {input}");

  let mut descendant = std::process::Command::new(std::env::current_exe().expect("fixture executable"));
  descendant
    .args(["--exact", FIXTURE_TEST_NAME, "--nocapture"])
    .env(FIXTURE_MODE_ENV, "descendant")
    .env(FIXTURE_HEARTBEAT_ENV, &heartbeat)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null());
  let mut descendant = descendant.spawn().expect("spawn fixture descendant");
  // Reap normally if the descendant ever exits before the leader. In the
  // scenarios under test the OS ownership boundary terminates both processes,
  // so this thread cannot weaken the descendant-survival assertion.
  thread::spawn(move || {
    let _ = descendant.wait();
  });
  wait_for_heartbeat_blocking(&heartbeat);

  match mode.to_str() {
    Some("success") => {},
    Some("error") => std::process::exit(17),
    Some("linger") => loop {
      thread::sleep(Duration::from_secs(1));
    },
    _ => panic!("unknown process fixture mode"),
  }
}

#[tokio::test]
async fn process_tree_pipes_io_and_owns_descendants_after_success_or_error() {
  for (mode, expected_success) in [("success", true), ("error", false)] {
    let temporary = tempfile::tempdir().unwrap();
    let heartbeat = temporary.path().join("heartbeat");
    let mut command = fixture_command(mode, &heartbeat);
    let mut tree = ProcessTree::spawn(&mut command).unwrap();
    let mut stdin = tree.take_stdin().unwrap();
    let mut stdout = tree.take_stdout().unwrap();
    let mut stderr = tree.take_stderr().unwrap();

    stdin.write_all(b"piped payload").await.unwrap();
    stdin.shutdown().await.unwrap();
    drop(stdin);
    let stdout_reader = tokio::spawn(async move {
      let mut bytes = Vec::new();
      stdout.read_to_end(&mut bytes).await.unwrap();
      bytes
    });
    let stderr_reader = tokio::spawn(async move {
      let mut bytes = Vec::new();
      stderr.read_to_end(&mut bytes).await.unwrap();
      bytes
    });

    let status = tokio::time::timeout(Duration::from_secs(5), tree.wait())
      .await
      .expect("fixture leader must exit")
      .unwrap();
    #[cfg(unix)]
    assert!(
      tree.process_group.is_none(),
      "wait retained a reusable process-group id"
    );
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    tree.close_exited_process_group().unwrap();
    assert_eq!(
      tree.wait().await.unwrap(),
      status,
      "repeated wait changed the exit status"
    );
    let (stdout, stderr) = tokio::join!(stdout_reader, stderr_reader);
    assert_eq!(status.success(), expected_success);
    assert!(String::from_utf8(stdout.unwrap())
      .unwrap()
      .contains("fixture stdout: piped payload"));
    assert!(String::from_utf8(stderr.unwrap())
      .unwrap()
      .contains("fixture stderr: piped payload"));
    assert_descendant_stopped(&heartbeat).await;
  }
}

#[tokio::test]
async fn dropping_the_process_owner_terminates_descendants() {
  let temporary = tempfile::tempdir().unwrap();
  let heartbeat = temporary.path().join("heartbeat");
  let mut command = fixture_command("linger", &heartbeat);
  let mut tree = ProcessTree::spawn(&mut command).unwrap();
  drop(tree.take_stdin().unwrap());
  let _stdout = tree.take_stdout().unwrap();
  let _stderr = tree.take_stderr().unwrap();
  wait_for_heartbeat(&heartbeat).await;

  // The command coordinator owns this value. Dropping its future therefore
  // exercises the same fail-safe path as dropping the plugin invocation.
  drop(tree);
  assert_descendant_stopped(&heartbeat).await;
}

fn fixture_command(mode: &str, heartbeat: &Path) -> Command {
  let mut command = Command::new(std::env::current_exe().expect("test executable"));
  command
    .args(["--exact", FIXTURE_TEST_NAME, "--nocapture"])
    .env(FIXTURE_MODE_ENV, mode)
    .env(FIXTURE_HEARTBEAT_ENV, heartbeat);
  command
}

fn wait_for_heartbeat_blocking(path: &Path) {
  let deadline = std::time::Instant::now() + Duration::from_secs(3);
  while std::time::Instant::now() < deadline {
    if heartbeat_size(path) > 0 {
      return;
    }
    thread::sleep(Duration::from_millis(10));
  }
  panic!("fixture descendant did not start");
}

async fn wait_for_heartbeat(path: &Path) {
  let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
  while tokio::time::Instant::now() < deadline {
    if heartbeat_size(path) > 0 {
      return;
    }
    tokio::time::sleep(Duration::from_millis(10)).await;
  }
  panic!("fixture descendant did not start");
}

async fn assert_descendant_stopped(path: &Path) {
  tokio::time::sleep(Duration::from_millis(100)).await;
  let first = heartbeat_size(path);
  tokio::time::sleep(Duration::from_millis(200)).await;
  assert_eq!(first, heartbeat_size(path), "owned descendant remained alive");
}

fn heartbeat_size(path: &Path) -> u64 {
  std::fs::metadata(path).map_or(0, |metadata| metadata.len())
}
