use std::{
  env,
  fs::{self, File},
  io::Write,
  path::PathBuf,
};

use assert_cmd::Command;
use predicates::prelude::{predicate, PredicateBooleanExt};
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn validation_plugins_dir() -> PathBuf {
  if let Some(path) = env::var_os("OCTA_E2E_PLUGINS_DIR") {
    return PathBuf::from(path);
  }

  let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
  #[cfg(windows)]
  let plugin_names = ["octa_plugin_shell.exe", "octa_plugin_tpl.exe"];
  #[cfg(not(windows))]
  let plugin_names = ["octa_plugin_shell", "octa_plugin_tpl"];

  if plugin_names.iter().all(|name| target_dir.join(name).is_file()) {
    target_dir
  } else {
    env::current_dir()
      .unwrap()
      .join("../../plugins")
      .canonicalize()
      .unwrap()
  }
}

#[test]
fn test_no_octafile_file_discovered() {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut cmd = Command::cargo_bin("octa").unwrap();
  cmd.current_dir(tmp_dir.path());
  cmd.arg("echo");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.assert().failure().stderr(predicate::str::contains(
    "Octafile not found traversing to root directory",
  ));
}

#[test]
fn test_global_octafile() -> Result<(), Box<dyn std::error::Error>> {
  let home_dir = TempDir::new()?;
  let working_dir = TempDir::new()?;
  fs::write(
    home_dir.path().join("Octafile.yml"),
    r#"
version: 1
tasks:
  global-task:
    shell: echo global-task
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd
    .current_dir(working_dir.path())
    .args(["--global", "global-task"])
    .env("HOME", home_dir.path())
    .env("USERPROFILE", home_dir.path())
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success().stdout(predicate::str::contains("global-task"));

  Ok(())
}

#[test]
fn test_dir_option_searches_upward() -> Result<(), Box<dyn std::error::Error>> {
  let project_dir = TempDir::new()?;
  let working_dir = TempDir::new()?;
  let nested_dir = project_dir.path().join("backend").join("nested");
  fs::create_dir_all(&nested_dir)?;
  fs::write(
    project_dir.path().join("Octafile.yml"),
    r#"
version: 1
tasks:
  from-dir:
    shell: echo from-dir
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd
    .current_dir(working_dir.path())
    .args(["--dir", nested_dir.to_str().unwrap(), "from-dir"])
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success().stdout(predicate::str::contains("from-dir"));

  Ok(())
}

#[test]
fn test_dir_option_resolves_relative_octafile() -> Result<(), Box<dyn std::error::Error>> {
  let project_dir = TempDir::new()?;
  let working_dir = TempDir::new()?;
  let config_dir = project_dir.path().join("config");
  fs::create_dir(&config_dir)?;
  fs::write(
    config_dir.join("custom.yml"),
    r#"
version: 1
tasks:
  relative-file:
    shell: echo relative-file
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd
    .current_dir(working_dir.path())
    .args([
      "--dir",
      project_dir.path().to_str().unwrap(),
      "--octafile",
      "config/custom.yml",
      "relative-file",
    ])
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success().stdout(predicate::str::contains("relative-file"));

  Ok(())
}

#[test]
fn test_run_simple_task() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
    version: 1
    tasks:
      hello:
        shell: echo "hello world"
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("hello");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.assert().success().stdout(predicate::str::contains("hello world"));

  Ok(())
}

#[test]
fn test_global_default_plugin_for_short_commands() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("config.yml"), "default_plugin: tpl\n")?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1
tasks:
  render: rendered by the configured default plugin
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd
    .current_dir(tmp_dir.path())
    .args(["--config", "config.yml", "render"])
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("rendered by the configured default plugin"));

  Ok(())
}

#[test]
fn test_task_if_condition() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1

tasks:
  condition:
    tpl: exit 0

  runs:
    if: "{{ deps_result.condition }}"
    deps:
      - condition
    shell: echo executed > executed.txt

  skips:
    if: exit 1
    cmds:
      - echo skipped > skipped.txt
      - echo also-skipped > also-skipped.txt
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("runs");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success();

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("skips");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success();

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.args(["--force", "skips"]);
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success();

  assert!(tmp_dir.path().join("executed.txt").is_file());
  assert!(!tmp_dir.path().join("skipped.txt").exists());
  assert!(!tmp_dir.path().join("also-skipped.txt").exists());

  Ok(())
}

#[test]
fn test_task_condition_phases_and_evaluation_frequency() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("condition.tpl"), "condition passed")?;
  #[cfg(windows)]
  let false_condition = "exit /B 1";
  #[cfg(not(windows))]
  let false_condition = "false";
  #[cfg(windows)]
  let once_condition = "echo once>>once-checks.txt & exit /B 0";
  #[cfg(not(windows))]
  let once_condition = "echo once >> once-checks.txt; true";
  #[cfg(windows)]
  let reference_condition = "echo reference>>reference-checks.txt & exit /B 0";
  #[cfg(not(windows))]
  let reference_condition = "echo reference >> reference-checks.txt; true";
  #[cfg(windows)]
  let per_command_condition = "echo command>>command-checks.txt & if exist stop.txt (exit /B 1) else (exit /B 0)";
  #[cfg(not(windows))]
  let per_command_condition = "echo command >> command-checks.txt; test ! -f stop.txt";
  let octafile = format!(
    r#"
version: 1

tasks:
  dependency:
    shell: echo dependency>dependency.txt

  before-skips-deps:
    if:
      before_deps: "{false_condition}"
    deps:
      - dependency
    shell: echo task>task.txt

  once:
    if:
      after_deps: "{once_condition}"
    cmds:
      - shell: echo first>once-first.txt
      - shell: echo second>once-second.txt

  per-command:
    if:
      after_deps:
        shell: "{per_command_condition}"
        evaluate: per_command
    cmds:
      - shell: echo stop>stop.txt
      - shell: echo should-not-run>skipped-after-stop.txt

  referenced:
    cmds:
      - shell: echo first>reference-first.txt
      - shell: echo second>reference-second.txt

  reference-condition:
    cmds:
      - task: referenced
        if: "{reference_condition}"

  plugin-condition:
    if:
      tpl:
        file: condition.tpl
    cmds:
      - shell: echo task-plugin>task-plugin-condition.txt
      - shell: echo command-plugin>command-plugin-condition.txt
        if:
          tpl: command condition passed
"#
  );
  fs::write(tmp_dir.path().join("octafile.yml"), octafile)?;

  for task in [
    "before-skips-deps",
    "once",
    "per-command",
    "reference-condition",
    "plugin-condition",
  ] {
    let mut cmd = Command::cargo_bin("octa")?;
    cmd.current_dir(tmp_dir.path());
    cmd.arg(task);
    cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
    cmd.assert().success();
  }

  assert!(!tmp_dir.path().join("dependency.txt").exists());
  assert!(!tmp_dir.path().join("task.txt").exists());
  assert!(tmp_dir.path().join("once-first.txt").is_file());
  assert!(tmp_dir.path().join("once-second.txt").is_file());
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("once-checks.txt"))?
      .lines()
      .count(),
    1
  );
  assert!(tmp_dir.path().join("stop.txt").is_file());
  assert!(!tmp_dir.path().join("skipped-after-stop.txt").exists());
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("command-checks.txt"))?
      .lines()
      .count(),
    2
  );
  assert!(tmp_dir.path().join("reference-first.txt").is_file());
  assert!(tmp_dir.path().join("reference-second.txt").is_file());
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("reference-checks.txt"))?
      .lines()
      .count(),
    1
  );
  assert!(tmp_dir.path().join("task-plugin-condition.txt").is_file());
  assert!(tmp_dir.path().join("command-plugin-condition.txt").is_file());

  Ok(())
}

#[test]
fn test_command_execution_options() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  #[cfg(windows)]
  let false_condition = "exit /B 1";
  #[cfg(not(windows))]
  let false_condition = "false";
  #[cfg(windows)]
  let failing_command = "exit /B 7";
  #[cfg(not(windows))]
  let failing_command = "exit 7";
  let octafile = format!(
    r#"
version: 1

tasks:
  command-options:
    env:
      HIDDEN_VALUE: hidden-command-output
    cmds:
      - shell: echo skipped>skipped.txt
        if: "{false_condition}"
      - tpl: $HIDDEN_VALUE
        silent: true
      - shell: "{failing_command}"
        ignore_error: true
      - shell: echo continued>continued.txt
"#
  );
  fs::write(tmp_dir.path().join("octafile.yml"), octafile)?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("command-options");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("hidden-command-output").not());

  assert!(!tmp_dir.path().join("skipped.txt").exists());
  assert!(tmp_dir.path().join("continued.txt").is_file());

  Ok(())
}

#[test]
fn test_deferred_commands() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  // Keep redirection adjacent to the echoed value because cmd.exe preserves a space before `>` in the output.
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1

tasks:
  success:
    cmds:
      - defer: echo first>>success.txt
      - defer:
          shell: echo second>>success.txt
      - defer: echo skipped>>success.txt
        platforms: [unsupported]
      - shell: echo work>>success.txt

  failure:
    cmds:
      - defer:
          task: cleanup
      - shell: echo work>>failure.txt
      - shell: exit 1

  cleanup:
    shell: echo cleanup>>failure.txt

  late_defer:
    cmds:
      - shell: exit 1
      - defer: echo late>late.txt

  cleanup_failure:
    cmds:
      - defer: exit 1
      - shell: echo success>cleanup-failure.txt

  nested:
    cmds:
      - defer: echo inner-cleanup>>nested.txt
      - shell: echo inner-work>>nested.txt

  outer:
    cmds:
      - defer: echo outer-cleanup>>nested.txt
      - task: nested
      - shell: echo outer-work>>nested.txt

  dependency:
    cmds:
      - defer: echo dependency-cleanup>>dependency.txt
      - shell: echo dependency-work>>dependency.txt

  with_dependency:
    deps: [dependency]
    shell: echo parent-work>>dependency.txt

  only_defer:
    deps: [prepare]
    cmds:
      - defer: echo cleanup>>only-defer.txt

  prepare:
    shell: echo prepare>>only-defer.txt

  failing_dependency:
    cmds:
      - defer: echo dependency-cleanup>failed-dependency.txt
      - shell: exit 1

  with_failing_dependency:
    deps: [failing_dependency]
    cmds:
      - defer: echo parent-cleanup>failed-parent.txt
      - shell: echo parent
"#,
  )?;

  let run = |task: &str| -> Result<Command, Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command.current_dir(tmp_dir.path());
    command.arg(task);
    command.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
    Ok(command)
  };

  run("success")?.assert().success();
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("success.txt"))?
      .lines()
      .collect::<Vec<_>>(),
    vec!["work", "second", "first"]
  );

  run("failure")?.assert().failure();
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("failure.txt"))?
      .lines()
      .collect::<Vec<_>>(),
    vec!["work", "cleanup"]
  );

  run("late_defer")?.assert().failure();
  assert!(!tmp_dir.path().join("late.txt").exists());

  run("cleanup_failure")?.assert().success();
  assert!(tmp_dir.path().join("cleanup-failure.txt").is_file());

  run("outer")?.assert().success();
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("nested.txt"))?
      .lines()
      .collect::<Vec<_>>(),
    vec!["inner-work", "inner-cleanup", "outer-work", "outer-cleanup"]
  );

  run("with_dependency")?.assert().success();
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("dependency.txt"))?
      .lines()
      .collect::<Vec<_>>(),
    vec!["dependency-work", "dependency-cleanup", "parent-work"]
  );

  run("only_defer")?.assert().success();
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("only-defer.txt"))?
      .lines()
      .collect::<Vec<_>>(),
    vec!["prepare", "cleanup"]
  );

  run("with_failing_dependency")?.assert().failure();
  assert!(tmp_dir.path().join("failed-dependency.txt").is_file());
  assert!(!tmp_dir.path().join("failed-parent.txt").exists());

  Ok(())
}

#[test]
fn test_run_default_task_when_task_is_not_specified() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  default:
    shell: echo "default task"
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success().stdout(predicate::str::contains("default task"));

  Ok(())
}

#[test]
fn test_missing_default_task_prints_help() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  build:
    shell: echo build
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("Usage: octa"))
    .stderr(predicate::str::contains("Command not found: default").not());

  Ok(())
}

#[test]
fn test_run_annotated_plugin_task() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let package_root = env::current_dir()?.join("../../plugins").canonicalize()?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  hello: !shell echo "hello from annotation"
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("hello");
  cmd.env("OCTA_PLUGINS_DIR", package_root);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("hello from annotation"));

  Ok(())
}

#[test]
fn test_run_template_plugin_task() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  hello:
    vars:
      name: World
    tpl: "Hello, {{ name }}!"
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("hello");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success().stdout(predicate::str::contains("Hello, World!"));

  Ok(())
}

#[test]
fn test_run_template_plugin_file() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("greeting.tpl"), "Hello from {{ source }}!")?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  direct:
    vars:
      source: file
    tpl:
      file: greeting.tpl

  command:
    vars:
      source: command
    cmds:
      - tpl:
          file: greeting.tpl
"#,
  )?;

  for (task, expected) in [("direct", "Hello from file!"), ("command", "Hello from command!")] {
    let mut cmd = Command::cargo_bin("octa")?;
    cmd.current_dir(tmp_dir.path());
    cmd.arg(task);
    cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
    cmd.assert().success().stdout(predicate::str::contains(expected));
  }

  Ok(())
}

#[test]
fn test_builtin_plugin_schemas_reject_invalid_values() -> Result<(), Box<dyn std::error::Error>> {
  let cases = [
    (
      "shell",
      r#"
version: 1
tasks:
  invalid:
    cmds:
      - shell:
          command: echo invalid
"#,
    ),
    (
      "tpl",
      r#"
version: 1
tasks:
  invalid: !tpl 42
"#,
    ),
  ];

  for (plugin, octafile) in cases {
    let tmp_dir = TempDir::new()?;
    fs::write(tmp_dir.path().join("octafile.yml"), octafile)?;

    let mut cmd = Command::cargo_bin("octa")?;
    cmd.current_dir(tmp_dir.path());
    cmd.arg("invalid");
    cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
    cmd.assert().failure().stderr(predicate::str::contains(format!(
      "invalid parameters for plugin '{plugin}'"
    )));
  }

  Ok(())
}

#[test]
fn test_octaignore_excludes_sources() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let package_root = env::current_dir()?.join("../../plugins").canonicalize()?;
  let src_dir = tmp_dir.path().join("src");
  let runs_file = tmp_dir.path().join("runs.txt");
  fs::create_dir(&src_dir)?;
  fs::write(src_dir.join("tracked.txt"), "tracked")?;
  fs::write(src_dir.join("ignored.txt"), "ignored")?;
  fs::write(src_dir.join(".octaignore"), "ignored.txt\n")?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  build:
    sources:
      - ./src/*.txt
    shell: echo run >> runs.txt
"#,
  )?;

  let run = || -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("octa")?;
    cmd.current_dir(tmp_dir.path());
    cmd.env("OCTA_TESTS", "");
    cmd.env("OCTA_PLUGINS_DIR", &package_root);
    cmd.arg("build");
    cmd.assert().success();
    Ok(())
  };

  run()?;
  assert_eq!(fs::read_to_string(&runs_file)?.lines().count(), 1);

  fs::write(src_dir.join("ignored.txt"), "ignored change")?;
  run()?;
  assert_eq!(fs::read_to_string(&runs_file)?.lines().count(), 1);

  fs::write(src_dir.join("tracked.txt"), "tracked change")?;
  run()?;
  assert_eq!(fs::read_to_string(&runs_file)?.lines().count(), 2);

  Ok(())
}

#[test]
fn test_task_args() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1
      tasks:
        hello:
          shell: echo {{ COMMAND_ARGS }}
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
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
fn test_ordered_and_secret_variables() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

vars:
  PREFIX: release
  VERSION: "{{ PREFIX }}-1"
  TOKEN:
    value: "{{ VERSION }}-token"
    secret: true

tasks:
  print:
    shell: echo {{ TOKEN }}
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd
    .current_dir(tmp_dir.path())
    .args(["print"])
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());

  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("release-1-token"));

  Ok(())
}

#[test]
fn test_file_option() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("sample.octafile.yml"))?;
  file.write_all(
    r#"
    version: 1

    tasks:
      hello:
        shell: echo Test
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.args(["-o=sample.octafile.yml", "hello"]);

  let output = cmd.output().expect("Failed to execute command");
  assert!(output.status.success());

  drop(file);
  drop(tmp_dir);
  Ok(())
}

#[test]
fn test_run_os_task() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
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
        shell: echo hello windows

      hello_linux:
        platforms: ['linux']
        shell: echo hello linux

      hello_macos:
        platforms: ['macos']
        shell: echo hello macos
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
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("hello");
  let output = cmd.output().expect("Failed to execute command");
  let stdout = String::from_utf8(output.stdout).expect("Invalid UTF-8 in stdout");
  assert!(
    stdout.contains(expected),
    "Missing found '{}' in stdout. Stdout: {}",
    expected,
    stdout
  );

  Ok(())
}

#[test]
fn test_platform_specific_commands() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let platform = if cfg!(target_os = "windows") {
    "windows"
  } else if cfg!(target_os = "macos") {
    "darwin"
  } else {
    "linux"
  };
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    format!(
      r#"
version: 1

tasks:
  build:
    cmds:
      - shell: echo plugin > plugin.txt
        platforms: [{platform}]
      - shell: echo skipped > skipped-plugin.txt
        platforms: [unsupported]
      - task: package
        platforms: [{platform}]
      - task: skipped
        platforms: [unsupported]

  package:
    shell: echo task > task.txt

  skipped:
    shell: echo skipped > skipped-task.txt

  skipped_all:
    cmds:
      - shell: echo skipped > skipped-all.txt
        platforms: [unsupported]
"#,
    ),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("build");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success();

  assert!(tmp_dir.path().join("plugin.txt").is_file());
  assert!(tmp_dir.path().join("task.txt").is_file());
  assert!(!tmp_dir.path().join("skipped-plugin.txt").exists());
  assert!(!tmp_dir.path().join("skipped-task.txt").exists());

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("skipped_all");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success();

  assert!(!tmp_dir.path().join("skipped-all.txt").exists());

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
          shell: echo %greeting%

        hello_linux_macos:
          platforms: ['macos', 'linux']
          shell: "echo $greeting"
    "#
    .as_bytes(),
  )?;

  let package_root = env!("CARGO_MANIFEST_DIR");
  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", format!("{}/../../plugins", package_root));
  cmd.arg("hello");

  cmd.assert().success().stdout(predicate::str::contains("hello world"));

  Ok(())
}

#[test]
fn test_env_file() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
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
          shell: "echo %VAR1%"

        test_linux_macos:
          platforms: ['macos', 'linux']
          shell: "echo $VAR1"
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("test");
  cmd.assert().success().stdout(predicate::str::contains("VAL1"));

  Ok(())
}

#[test]
fn test_explicit_env_files() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let env_dir = tmp_dir.path().join("config");
  fs::create_dir(&env_dir)?;
  fs::write(tmp_dir.path().join(".env"), "OCTA_E2E_DOTENV_SHARED=default\n")?;
  fs::write(
    env_dir.join("first.env"),
    "OCTA_E2E_DOTENV_FIRST=first\nOCTA_E2E_DOTENV_SHARED=first\n",
  )?;
  fs::write(
    env_dir.join("second.env"),
    "OCTA_E2E_DOTENV_SECOND=second\nOCTA_E2E_DOTENV_SHARED=second\nOCTA_E2E_DOTENV_PROCESS=file\n",
  )?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  show:
    tpl: "$OCTA_E2E_DOTENV_FIRST|$OCTA_E2E_DOTENV_SECOND|$OCTA_E2E_DOTENV_SHARED|$OCTA_E2E_DOTENV_PROCESS"
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.args([
    "--env-file",
    "config/first.env",
    "--env-file",
    "config/second.env",
    "show",
  ]);
  cmd.env("OCTA_E2E_DOTENV_PROCESS", "process");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("first|second|second|process"));

  Ok(())
}

#[test]
fn test_octafile_and_task_dotenv() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::create_dir(tmp_dir.path().join("nested"))?;
  fs::write(tmp_dir.path().join(".env.test"), "ROOT_VALUE=root\n")?;
  fs::write(tmp_dir.path().join(".env.task"), "TASK_VALUE=task\nEXPLICIT=dotenv\n")?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
vars:
  PROFILE: test
dotenv:
  - .env.{{ PROFILE }}
tasks:
  show:
    dir: nested
    dotenv:
      - .env.task
    env:
      EXPLICIT: task
    tpl: "$ROOT_VALUE|$TASK_VALUE|$EXPLICIT"
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("show");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("root|task|task"));

  Ok(())
}

#[test]
fn test_shell_backed_environment_values() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let task_dir = tmp_dir.path().join("nested");
  fs::create_dir(&task_dir)?;
  fs::write(task_dir.join("value.txt"), "dynamic")?;
  fs::write(task_dir.join(".env.runtime"), "PREFIX=from-dotenv\n")?;
  #[cfg(windows)]
  let read_command = "type value.txt";
  #[cfg(not(windows))]
  let read_command = "cat value.txt";
  #[cfg(windows)]
  let env_command = "echo %PREFIX%";
  #[cfg(not(windows))]
  let env_command = "echo $PREFIX";
  let octafile = format!(
    r#"
version: 1
tasks:
  show:
    dir: nested
    dotenv:
      - .env.runtime
    vars:
      VERSION:
        sh: "{read_command}"
    env:
      FROM_VAR: "{{{{ VERSION }}}}"
      FROM_SHELL:
        sh: "{env_command}"
      FROM_FILTER: '{{{{ "{read_command}" | shell }}}}'
    tpl: "$FROM_VAR|$FROM_SHELL|$FROM_FILTER"
"#
  );
  fs::write(tmp_dir.path().join("octafile.yml"), octafile)?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("show");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("dynamic|from-dotenv|dynamic"));

  Ok(())
}

#[test]
fn test_shell_environment_is_evaluated_once_per_execution() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::create_dir(tmp_dir.path().join("nested"))?;
  #[cfg(windows)]
  let env_command = "echo call>>../calls.txt & echo value";
  #[cfg(not(windows))]
  let env_command = "echo call >> ../calls.txt; echo value";
  #[cfg(windows)]
  let condition = "exit /B 0";
  #[cfg(not(windows))]
  let condition = "true";
  let octafile = format!(
    r#"
version: 1
tasks:
  show:
    dir: nested
    if: "{condition}"
    env:
      VALUE:
        sh: "{env_command}"
    tpl: "$VALUE"
"#
  );
  fs::write(tmp_dir.path().join("octafile.yml"), octafile)?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("show");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success().stdout(predicate::str::contains("value"));

  let calls = fs::read_to_string(tmp_dir.path().join("calls.txt"))?;
  assert_eq!(calls.lines().count(), 1);

  Ok(())
}

#[test]
fn test_missing_explicit_env_file() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.args(["--env-file", "config/missing.env", "--list-tasks"]);
  cmd
    .assert()
    .failure()
    .stderr(predicate::str::contains("config/missing.env"));

  Ok(())
}

#[test]
fn test_dry_run() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1
      tasks:
        test:
          shell: touch test.txt
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.args(["--dry", "test"]);

  cmd.assert().success();

  assert!(
    !tmp_dir.path().join("test.txt").exists(),
    "File should not be created in dry run mode"
  );

  Ok(())
}

#[test]
fn test_dry_run_does_not_execute_plugin_condition() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  guarded:
    if:
      tpl:
        file: missing-condition.tpl
    shell: echo guarded
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.args(["--dry", "guarded"]);

  cmd.assert().success();

  Ok(())
}

#[test]
fn test_create_task_directory() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let task_dir = tmp_dir.path().join("build").join("generated");
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  build:
    vars:
      OUTPUT_DIR: build/generated
    dir: "{{ OUTPUT_DIR }}"
    shell: echo created > result.txt
"#,
  )?;

  let plugins_dir = validation_plugins_dir();
  let mut dry_run = Command::cargo_bin("octa")?;
  dry_run.current_dir(tmp_dir.path());
  dry_run.env("OCTA_PLUGINS_DIR", &plugins_dir);
  dry_run.args(["--dry", "build"]);
  dry_run.assert().success();
  assert!(!task_dir.exists());

  let mut run = Command::cargo_bin("octa")?;
  run.current_dir(tmp_dir.path());
  run.env("OCTA_PLUGINS_DIR", plugins_dir);
  run.arg("build");
  run.assert().success();

  assert!(task_dir.join("result.txt").is_file());

  Ok(())
}

#[test]
fn test_task_run_mode() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
    version: 1
    run: changed
    tasks:
      long:
        run: once
        shell: sleep 1

      task:
        shell: echo {{ CONTENT }}
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
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("test");

  let output = cmd.output().expect("Failed to execute command");
  assert!(
    output.status.success(),
    "stdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

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
fn test_parallel_execution() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1
      tasks:
        parallel_test:
          cmds:
            - task: task1
            - task: task2
            - task: task3

        task1:
          shell: echo "task1"

        task2:
          shell: echo "task2"

        task3:
          shell: echo "task3"
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.args(["--parallel", "--concurrency", "2", "parallel_test"]);

  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("task1"))
    .stdout(predicate::str::contains("task2"))
    .stdout(predicate::str::contains("task3"));

  Ok(())
}

#[test]
fn test_list_tasks() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1
      tasks:
        task1:
          desc: "First task"
          shell: echo "task1"

        task2:
          desc: "Second task"
          shell: echo "task2"

        _internal:
          internal: true
          shell: echo "internal"
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("--list-tasks");

  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("task1: First task"))
    .stdout(predicate::str::contains("task2: Second task"))
    .stdout(predicate::str::contains("_internal").not());

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.args(["--search", "SECOND"]);

  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("task2: Second task"))
    .stdout(predicate::str::contains("task1").not())
    .stdout(predicate::str::contains("_internal").not());

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.args(["--search", "internal"]);
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("_internal").not());

  Ok(())
}

#[test]
fn test_clean_cache() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1
      tasks:
        test:
          run: changed
          shell: echo "test"
    "#
    .as_bytes(),
  )?;

  // Run task first time
  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("test");
  cmd.assert().success();

  // Clean cache
  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("--clean-cache");
  cmd.assert().success();

  // Run task again - should execute because cache was cleaned
  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("test");
  cmd.assert().success().stdout(predicate::str::contains("test"));

  Ok(())
}

#[test]
fn test_force_execution() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
  let mut file = File::create(tmp_dir.path().join("octafile.yml"))?;
  file.write_all(
    r#"
      version: 1
      tasks:
        test:
          run: changed
          shell: echo "forced"
    "#
    .as_bytes(),
  )?;

  // Run task first time
  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("test");
  cmd.assert().success();

  // Run task with force flag
  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.args(["--force", "test"]);
  cmd.assert().success().stdout(predicate::str::contains("forced"));

  Ok(())
}

#[test]
fn test_comple_executor_plan() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new().unwrap();
  let package_root = env::current_dir().unwrap().join("../../plugins");
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
        shell: echo {{ CONTENT }}

      yyy:
        vars:
          CONTENT: YYY
        shell: echo {{ CONTENT }}
        deps:
          - nnn

      nnn:
        shell: echo NNN
    "#
    .as_bytes(),
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.env("OCTA_TESTS", "");
  cmd.env("OCTA_PLUGINS_DIR", package_root.canonicalize().unwrap());
  cmd.arg("zzz");

  let output = cmd.output().expect("Failed to execute command");

  let stderr = String::from_utf8_lossy(&output.stderr);
  assert_eq!(stderr, "");

  assert_eq!(output.status.success(), true);

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
