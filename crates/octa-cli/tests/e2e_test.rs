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
  cmd
    .assert()
    .failure()
    .stderr(predicate::str::contains("OctafileLoad(NotSearchedError)"));
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
