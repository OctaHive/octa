use std::{fs::File, io::Write};

use assert_cmd::Command;
use predicates::prelude::predicate;
use tempfile::TempDir;

#[test]
fn test_no_octafile_file_discovered() {
  let tmp_dir = TempDir::new().unwrap();
  let mut cmd = Command::cargo_bin("octa").unwrap();
  cmd.current_dir(tmp_dir.path());
  cmd.arg("echo");
  cmd.assert().failure().stderr(predicate::str::contains(
    "Octafile not found traversing to root directory",
  ));
}

#[test]
fn test_run_simple_task() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
    version: 1
    tasks:
      hello:
        cmd: echo "hello world"
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("hello");
  cmd.assert().success().stdout(predicate::str::contains("hello world"));

  Ok(())
}

#[test]
fn test_task_args() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1
      tasks:
        hello:
          cmd: echo {{ COMMAND_ARGS }}
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.arg("hello");
  cmd.arg("--");
  cmd.arg("arg1");
  cmd.arg("arg2");

  let output = cmd.output().expect("Failed to execute command");
  assert!(output.status.success());

  let stdout = String::from_utf8(output.stdout).expect("Invalid UTF-8 in stdout");
  let lines: Vec<&str> = stdout.lines().collect();
  let expected_lines = vec![
    "Building DAG for command hello with provided args [\"arg1\", \"arg2\"]",
    "Starting execution plan for command hello",
    "Starting task hello",
    "[arg1, arg2]",
    "All tasks completed successfully",
  ];

  assert_eq!(lines, expected_lines);

  Ok(())
}

#[test]
fn test_file_option() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let mut file = File::create(tmp_dir.path().join("sample.octafile.yml"))?;
  file.write_all(
    r#"
    version: 1

    tasks:
      hello:
        cmd: echo Test
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.args(["-c=sample.octafile.yml", "hello"]);

  let output = cmd.output().expect("Failed to execute command");
  assert!(output.status.success());

  drop(file);
  drop(tmp_dir);
  Ok(())
}

#[test]
fn test_run_os_task() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
    version: 1

    tasks:
      hello:
        deps:
          - hello_windows
          - hello_linux
          - hello_macos

      hello_windows:
        platforms: ['windows']
        cmd: echo hello windows

      hello_linux:
        platforms: ['linux']
        cmd: echo hello linux

      hello_macos:
        platforms: ['macos']
        cmd: echo hello macos
    "#
    .as_bytes(),
  )?;

  let expected = if cfg!(target_os = "windows") {
    "hello windows"
  } else if cfg!(target_os = "linux") {
    "hello linux"
  } else {
    "hello macos"
  };

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.arg("hello");
  cmd.assert().success().stdout(predicate::str::contains(expected));

  Ok(())
}

#[test]
#[ignore]
fn test_set_env() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1

      env:
        greeting: "hello world"

      tasks:
        hello:
          deps:
            - hello_windows
            - hello_linux_macos

        hello_windows:
          platforms: ['windows']
          cmd: echo %greeting%

        hello_linux_macos:
          platforms: ['macos', 'linux']
          cmd: "echo $greeting"
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.arg("hello");

  cmd.assert().success().stdout(predicate::str::contains("hello world"));

  Ok(())
}

#[test]
fn test_env_file() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let mut env_file = File::create(tmp_dir.path().join(".env"))?;
  env_file
    .write_all(
      r#"
        VAR1=VAL1
      "#
      .as_bytes(),
    )
    .unwrap();

  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1

      tasks:
        test:
          deps:
            - test_windows
            - test_linux_macos

        test_windows:
          platforms: ['windows']
          cmd: "echo %VAR1%"

        test_linux_macos:
          platforms: ['macos', 'linux']
          cmd: "echo $VAR1"
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.arg("test");
  cmd.assert().success().stdout(predicate::str::contains("VAL1"));

  Ok(())
}

#[test]
fn test_task_run_mode() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
    version: 1
    tasks:
      long:
        run: once
        cmd: sleep 1

      task:
        run: changed
        cmd: echo {{ CONTENT }}
        deps:
          - long

      test:
        cmds:
          - task: task
            vars:
              CONTENT: 1
          - task: task
            vars:
              CONTENT: 2
          - task: task
            vars:
              CONTENT: 2
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.arg("test");

  let output = cmd.output().expect("Failed to execute command");
  assert!(output.status.success());

  let stdout = String::from_utf8(output.stdout).expect("Invalid UTF-8 in stdout");
  let lines: Vec<&str> = stdout.lines().collect();
  let expected_lines = vec![
    "Building DAG for command test with provided args []",
    "Starting execution plan for command test",
    "Starting task long",
    "Starting task task",
    "1",
    "Starting task task",
    "2",
    "All tasks completed successfully",
  ];

  assert_eq!(lines, expected_lines);

  Ok(())
}

#[test]
fn test_comple_executor_plan() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
    version: 1
    tasks:
      zzz:
        cmds:
          - task: yyy
          - echo PoW
          - task: yyy
            vars:
              CONTENT: Psih
        deps:
          - task: www
            vars:
              CONTENT: WWW

      www:
        cmds:
          - task: content
            vars:
              CONTENT: MMMM
          - echo {{ CONTENT }}

      content:
        cmd: echo {{ CONTENT }}

      yyy:
        vars:
          CONTENT: YYY
        cmd: echo {{ CONTENT }}
        deps:
          - nnn

      nnn:
        cmd: echo NNN
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.arg("zzz");

  let output = cmd.output().expect("Failed to execute command");
  assert!(output.status.success());

  let stdout = String::from_utf8(output.stdout).expect("Invalid UTF-8 in stdout");
  let lines: Vec<&str> = stdout.lines().collect();
  let expected_lines = vec![
    "Building DAG for command zzz with provided args []",
    "Starting execution plan for command zzz",
    "Starting task content",
    "MMMM",
    "Starting task echo {{ CONTENT }}",
    "WWW",
    "Starting task nnn",
    "NNN",
    "Starting task yyy",
    "YYY",
    "Starting task echo PoW",
    "PoW",
    "Starting task nnn",
    "NNN",
    "Starting task yyy",
    "Psih",
    "All tasks completed successfully",
  ];

  assert_eq!(lines, expected_lines);

  Ok(())
}
