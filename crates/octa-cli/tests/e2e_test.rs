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
fn test_monorepo_tasks_use_colon_namespaces() -> Result<(), Box<dyn std::error::Error>> {
  let workspace = TempDir::new()?;
  let api_dir = workspace.path().join("packages/api");
  let web_dir = workspace.path().join("packages/web");
  fs::create_dir_all(&api_dir)?;
  fs::create_dir_all(&web_dir)?;
  fs::write(
    workspace.path().join("Octafile.yml"),
    r#"
version: 1
monorepo:
  roots:
    - packages/*
tasks:
  root:
    shell: echo root
"#,
  )?;
  fs::write(
    api_dir.join("Octafile.yml"),
    "version: 1\ntasks:\n  build:\n    shell: echo api-build\n",
  )?;
  fs::write(
    web_dir.join("Octafile.yml"),
    "version: 1\ntasks:\n  build:\n    shell: echo web-build\n",
  )?;

  let mut run = Command::cargo_bin("octa")?;
  run
    .current_dir(workspace.path())
    .args(["packages:api:build"])
    .env("OCTA_CACHE_DIR", workspace.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  run.assert().success().stdout(predicate::str::contains("api-build"));

  let mut list = Command::cargo_bin("octa")?;
  list
    .current_dir(workspace.path())
    .arg("--list-tasks")
    .env("OCTA_CACHE_DIR", workspace.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  list
    .assert()
    .success()
    .stdout(predicate::str::contains("packages:api:build"))
    .stdout(predicate::str::contains("packages:web:build"))
    .stdout(predicate::str::contains("packages/api").not());

  Ok(())
}

#[test]
fn test_monorepo_uses_the_current_project_for_bare_task_names() -> Result<(), Box<dyn std::error::Error>> {
  let workspace = TempDir::new()?;
  let api_dir = workspace.path().join("packages/api");
  let nested_dir = api_dir.join("src/nested");
  fs::create_dir_all(&nested_dir)?;
  fs::write(
    workspace.path().join("Octafile.yml"),
    "version: 1\nmonorepo:\n  roots: [packages/**]\ntasks: {}\n",
  )?;
  fs::write(
    api_dir.join("Octafile.yml"),
    "version: 1\ntasks:\n  build:\n    shell: echo current-project\n",
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(&nested_dir)
    .arg("build")
    .env("OCTA_CACHE_DIR", workspace.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  command
    .assert()
    .success()
    .stdout(predicate::str::contains("current-project"));

  let mut explicit = Command::cargo_bin("octa")?;
  explicit
    .current_dir(&api_dir)
    .args(["--octafile", "Octafile.yml", "build"])
    .env("OCTA_CACHE_DIR", workspace.path().join("explicit-cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  explicit
    .assert()
    .success()
    .stdout(predicate::str::contains("current-project"));

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
fn test_summary_is_printed_after_execution() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  build:\n    shell: echo built\n",
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .args(["--summary", "build"])
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  command
    .assert()
    .success()
    .stdout(predicate::str::contains("Time Summary"))
    .stdout(predicate::str::contains("build"))
    .stdout(predicate::str::contains("Total time"));

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
  let false_condition = "false";
  let once_condition = "echo once >> once-checks.txt; true";
  let reference_condition = "echo reference >> reference-checks.txt; true";
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
  let false_condition = "false";
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

  success_status:
    cmds:
      - defer: 'echo "{% if EXIT_CODE is defined %}set{% else %}unset{% endif %}">success-status.txt'
      - shell: exit 0

  failure:
    cmds:
      - defer:
          task: cleanup
          vars:
            CODE: "{{ EXIT_CODE }}"
      - shell: echo work>>failure.txt
      - shell: exit 23

  cleanup:
    shell: echo cleanup-{{ CODE }}>>failure.txt

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

  run("success_status")?.assert().success();
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("success-status.txt"))?.trim(),
    "unset"
  );

  run("failure")?.assert().failure();
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("failure.txt"))?
      .lines()
      .collect::<Vec<_>>(),
    vec!["work", "cleanup-23"]
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
fn test_output_controls_the_whole_task_freshness() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let source = tmp_dir.path().join("source.txt");
  let ignored_source = tmp_dir.path().join("source.generated.txt");
  let output = tmp_dir.path().join("artifact.txt");
  let ignored_output = tmp_dir.path().join("artifact.debug.txt");
  let first_runs = tmp_dir.path().join("first-runs.txt");
  let last_runs = tmp_dir.path().join("last-runs.txt");
  fs::write(&source, "initial")?;
  fs::write(&ignored_source, "initial")?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  build:
    sources:
      - ./source*.txt
      - "!./source.generated.txt"
    output:
      - ./artifact*.txt
      - "!./artifact.debug.txt"
    cmds:
      - echo first>>first-runs.txt
      - echo artifact>artifact.txt
      - echo debug>artifact.debug.txt
      - echo last>>last-runs.txt
"#,
  )?;

  let run = || -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("build");
    command.assert().success();
    Ok(())
  };

  run()?;
  fs::write(ignored_source, "ignored change")?;
  fs::remove_file(ignored_output)?;
  run()?;
  assert_eq!(fs::read_to_string(&first_runs)?.lines().count(), 1);
  assert_eq!(fs::read_to_string(&last_runs)?.lines().count(), 1);

  fs::remove_file(&output)?;
  run()?;
  assert_eq!(fs::read_to_string(&first_runs)?.lines().count(), 2);
  assert_eq!(fs::read_to_string(&last_runs)?.lines().count(), 2);

  fs::write(source, "changed")?;
  run()?;
  assert_eq!(fs::read_to_string(&first_runs)?.lines().count(), 3);
  assert_eq!(fs::read_to_string(&last_runs)?.lines().count(), 3);

  Ok(())
}

#[test]
fn test_failed_task_does_not_commit_its_source_fingerprint() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let runs = tmp_dir.path().join("runs.txt");
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  let failing_command = "echo run>>runs.txt && exit 1";
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    format!(
      r#"
version: 1

tasks:
  build:
    sources:
      - ./source.txt
    output: []
    shell: {failing_command}
"#,
    ),
  )?;

  for _ in 0..2 {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("build");
    command.assert().failure();
  }

  assert_eq!(fs::read_to_string(runs)?.lines().count(), 2);
  Ok(())
}

#[test]
fn test_skipped_task_does_not_commit_its_source_fingerprint() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let runs = tmp_dir.path().join("runs.txt");
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  let condition = "test -f enabled.txt";
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    format!(
      r#"
version: 1

tasks:
  build:
    if: '{condition}'
    sources:
      - ./source.txt
    output: []
    shell: echo run>>runs.txt
"#,
    ),
  )?;

  let run = || -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("build");
    command.assert().success();
    Ok(())
  };

  run()?;
  assert!(!runs.exists());

  fs::write(tmp_dir.path().join("enabled.txt"), "")?;
  run()?;
  assert_eq!(fs::read_to_string(runs)?.lines().count(), 1);

  Ok(())
}

#[test]
fn test_command_condition_keeps_task_stale_until_all_commands_run() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  let condition = "test -f enabled.txt";
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    format!(
      r#"
version: 1

tasks:
  build:
    sources:
      - ./source.txt
    output: []
    cmds:
      - echo always>>always-runs.txt
      - shell: echo conditional>>conditional-runs.txt
        if: '{condition}'
"#,
    ),
  )?;

  let run = || -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("build");
    command.assert().success();
    Ok(())
  };

  run()?;
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("always-runs.txt"))?
      .lines()
      .count(),
    1
  );
  assert!(!tmp_dir.path().join("conditional-runs.txt").exists());

  fs::write(tmp_dir.path().join("enabled.txt"), "")?;
  run()?;
  run()?;

  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("always-runs.txt"))?
      .lines()
      .count(),
    2
  );
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("conditional-runs.txt"))?
      .lines()
      .count(),
    1
  );
  Ok(())
}

#[test]
fn test_freshness_distinguishes_inline_variable_values() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  build:
    sources:
      - ./source.txt
    output: []
    shell: echo {{ VALUE }}>>runs.txt
"#,
  )?;

  let run = |value: &str| -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .args(["build", &format!("VALUE={value}")]);
    command.assert().success();
    Ok(())
  };

  run("one")?;
  run("two")?;
  run("two")?;

  assert_eq!(fs::read_to_string(tmp_dir.path().join("runs.txt"))?.lines().count(), 2);
  Ok(())
}

#[test]
fn test_referenced_task_freshness_is_independent_from_parent() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let child_runs = tmp_dir.path().join("child-runs.txt");
  fs::write(tmp_dir.path().join("parent.txt"), "initial")?;
  fs::write(tmp_dir.path().join("child.txt"), "initial")?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  parent:
    sources: [parent.txt]
    output: []
    cmds:
      - task: child

  child:
    sources: [child.txt]
    output: [child.out]
    cmds:
      - echo child>>child-runs.txt
      - echo artifact>child.out
"#,
  )?;

  let run = || -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("parent");
    command.assert().success();
    Ok(())
  };

  run()?;
  run()?;
  assert_eq!(fs::read_to_string(&child_runs)?.lines().count(), 1);

  fs::write(tmp_dir.path().join("child.txt"), "changed")?;
  run()?;
  assert_eq!(fs::read_to_string(&child_runs)?.lines().count(), 2);

  fs::write(tmp_dir.path().join("parent.txt"), "changed")?;
  run()?;
  assert_eq!(fs::read_to_string(&child_runs)?.lines().count(), 2);

  Ok(())
}

#[test]
fn test_up_to_date_parent_skips_nested_condition_gates_cleanly() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  fs::write(tmp_dir.path().join("enabled.txt"), "")?;
  let condition = "test -f enabled.txt";

  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    format!(
      r#"
version: 1

tasks:
  parent:
    sources: [source.txt]
    output: []
    cmds:
      - task: child

  child:
    if: '{condition}'
    sources: [source.txt]
    output: []
    shell: echo child>>runs.txt
"#,
    ),
  )?;

  for _ in 0..2 {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("parent");
    command.assert().success();
  }

  assert_eq!(fs::read_to_string(tmp_dir.path().join("runs.txt"))?.lines().count(), 1);
  Ok(())
}

#[test]
fn test_nested_task_definition_invalidates_its_own_freshness() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let runs = tmp_dir.path().join("runs.txt");
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  fs::write(tmp_dir.path().join("child-source.txt"), "child")?;

  let write_octafile = |message: &str| -> Result<(), std::io::Error> {
    fs::write(
      tmp_dir.path().join("Octafile.yml"),
      format!(
        r#"
version: 1

tasks:
  parent:
    sources: [source.txt]
    output: []
    cmds:
      - task: child

  child:
    sources: [child-source.txt]
    output: []
    shell: echo {message}>>runs.txt
"#,
      ),
    )
  };
  let run = || -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("parent");
    command.assert().success();
    Ok(())
  };

  write_octafile("first")?;
  run()?;
  run()?;
  assert_eq!(fs::read_to_string(&runs)?.lines().collect::<Vec<_>>(), ["first"]);

  write_octafile("second")?;
  run()?;
  assert_eq!(
    fs::read_to_string(&runs)?.lines().collect::<Vec<_>>(),
    ["first", "second"]
  );
  Ok(())
}

#[test]
fn test_unrelated_process_environment_does_not_invalidate_freshness() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  build:
    sources: [source.txt]
    output: []
    shell: echo run>>runs.txt
"#,
  )?;

  for value in ["first", "second"] {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .env("OCTA_UNRELATED", value)
      .arg("build");
    command.assert().success();
  }

  assert_eq!(fs::read_to_string(tmp_dir.path().join("runs.txt"))?.lines().count(), 1);
  Ok(())
}

#[test]
fn test_dynamic_freshness_inputs_are_resolved_once_and_reused_by_commands() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  fs::write(tmp_dir.path().join("dynamic-var.txt"), "one")?;
  fs::write(tmp_dir.path().join("dynamic-env.txt"), "env-one")?;

  let var_command = "echo var>>var-resolutions.txt && value=$(<dynamic-var.txt) && echo $value";
  let env_command = "echo env>>env-resolutions.txt && value=$(<dynamic-env.txt) && echo $value";
  let task_command = "echo {{ DYNAMIC_VAR }}-$DYNAMIC_ENV>>runs.txt";

  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    format!(
      r#"
version: 1

vars:
  DYNAMIC_VAR:
    sh: '{var_command}'

env:
  DYNAMIC_ENV:
    sh: '{env_command}'

tasks:
  build:
    sources: [source.txt]
    output: []
    cmds:
      - '{task_command}'
      - '{task_command}'
"#,
    ),
  )?;

  let run = || -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("build");
    command.assert().success();
    Ok(())
  };

  run()?;
  fs::write(tmp_dir.path().join("dynamic-var.txt"), "two")?;
  fs::write(tmp_dir.path().join("dynamic-env.txt"), "env-two")?;
  run()?;
  run()?;

  let runs = fs::read_to_string(tmp_dir.path().join("runs.txt"))?;
  assert_eq!(runs.lines().count(), 4);
  assert!(runs.lines().take(2).all(|line| line.contains("one-env-one")));
  assert!(runs.lines().skip(2).all(|line| line.contains("two-env-two")));
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("var-resolutions.txt"))?
      .lines()
      .count(),
    3
  );
  assert_eq!(
    fs::read_to_string(tmp_dir.path().join("env-resolutions.txt"))?
      .lines()
      .count(),
    3
  );
  Ok(())
}

#[test]
fn test_parent_becomes_current_when_a_nested_task_is_already_current() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("parent.txt"), "initial")?;
  fs::write(tmp_dir.path().join("child.txt"), "initial")?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  parent:
    sources: [parent.txt]
    output: []
    cmds:
      - task: child

  child:
    sources: [child.txt]
    output: [child.out]
    shell: echo child>child.out
"#,
  )?;

  let run = || -> Result<assert_cmd::assert::Assert, Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("parent");
    Ok(command.assert().success())
  };

  run()?;
  fs::write(tmp_dir.path().join("parent.txt"), "changed")?;
  run()?.stdout(predicate::str::contains("Task child is up to date"));
  run()?.stdout(predicate::str::contains("Task parent is up to date"));

  Ok(())
}

#[test]
fn test_freshness_identity_tracks_configuration_and_dotenv() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(tmp_dir.path().join("source.txt"), "source")?;
  fs::write(tmp_dir.path().join(".env"), "FROM_DOTENV=one\n")?;

  let write_octafile = |value: &str, marker: &str| -> Result<(), Box<dyn std::error::Error>> {
    let shell = format!("echo {{{{ VALUE }}}}-$FROM_DOTENV-{marker}>>runs.txt");
    fs::write(
      tmp_dir.path().join("Octafile.yml"),
      format!(
        r#"
version: 1

vars:
  VALUE: {value}

dotenv: [.env]

tasks:
  build:
    sources: [source.txt]
    output: []
    shell: {shell}
"#,
      ),
    )?;
    Ok(())
  };
  let run = || -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_TESTS", "")
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("build");
    command.assert().success();
    Ok(())
  };
  let run_count = || -> Result<usize, Box<dyn std::error::Error>> {
    Ok(fs::read_to_string(tmp_dir.path().join("runs.txt"))?.lines().count())
  };

  write_octafile("one", "stable")?;
  run()?;
  run()?;
  assert_eq!(run_count()?, 1);

  write_octafile("two", "stable")?;
  run()?;
  assert_eq!(run_count()?, 2);

  fs::write(tmp_dir.path().join(".env"), "FROM_DOTENV=two\n")?;
  run()?;
  assert_eq!(run_count()?, 3);

  write_octafile("two", "changed")?;
  run()?;
  assert_eq!(run_count()?, 4);

  write_octafile("one", "stable")?;
  fs::write(tmp_dir.path().join(".env"), "FROM_DOTENV=one\n")?;
  run()?;
  assert_eq!(run_count()?, 5);

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
fn test_inline_cli_variables() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

vars:
  OCTA_INLINE_TEST_VALUE: file

tasks:
  default:
    shell: echo "{{ DEFAULT_ONLY }}"

  print:
    shell: echo "{{ OCTA_INLINE_TEST_VALUE }}|{{ COMPOSED }}|{{ EMPTY }}|{{ WITH_EQUALS }}"
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd
    .current_dir(tmp_dir.path())
    .args([
      "--var",
      "OCTA_INLINE_TEST_VALUE=explicit",
      "OCTA_INLINE_TEST_VALUE=first",
      "print",
      "OCTA_INLINE_TEST_VALUE=cli",
      "COMPOSED={{ OCTA_INLINE_TEST_VALUE }}-suffix",
      "EMPTY=",
      "WITH_EQUALS=a=b",
    ])
    .env("OCTA_INLINE_TEST_VALUE", "environment")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());

  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("cli|cli-suffix||a=b"));

  let mut cmd = Command::cargo_bin("octa")?;
  cmd
    .current_dir(tmp_dir.path())
    .arg("DEFAULT_ONLY=default-value")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());

  cmd.assert().success().stdout(predicate::str::contains("default-value"));

  Ok(())
}

#[test]
fn test_required_variables() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

vars:
  GLOBAL_REQUIRED:
    required: true
  DISPLAY: "configured-{{ GLOBAL_REQUIRED }}"
  ENVIRONMENTS: [development, production]

tasks:
  deploy:
    vars:
      TASK_SECRET:
        required: true
        secret: true
    shell: echo "{{ DISPLAY }} {{ TASK_SECRET }}"

  select-environment:
    vars:
      ENVIRONMENT:
        required: prompt
        enum: [development, production]
    shell: echo "{{ ENVIRONMENT }}"

  select-dynamic-environment:
    vars:
      ENVIRONMENT:
        required: prompt
        enum: "{{ ENVIRONMENTS }}"
    shell: echo "{{ ENVIRONMENT }}"
"#,
  )?;

  let mut list = Command::cargo_bin("octa")?;
  list
    .current_dir(tmp_dir.path())
    .arg("--list-tasks")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  list.assert().success().stdout(predicate::str::contains("deploy"));

  let mut missing = Command::cargo_bin("octa")?;
  missing
    .current_dir(tmp_dir.path())
    .args(["deploy", "GLOBAL_REQUIRED=present"])
    .env_remove("TASK_SECRET")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  missing
    .assert()
    .failure()
    .stderr(predicate::str::contains("Required variable 'TASK_SECRET' is not set"));

  let mut supplied = Command::cargo_bin("octa")?;
  supplied
    .current_dir(tmp_dir.path())
    .args(["deploy", "TASK_SECRET=token"])
    .env("GLOBAL_REQUIRED", "environment")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  supplied
    .assert()
    .success()
    .stdout(predicate::str::contains("configured-environment token"));

  let mut empty = Command::cargo_bin("octa")?;
  empty
    .current_dir(tmp_dir.path())
    .args(["deploy", "GLOBAL_REQUIRED=present", "TASK_SECRET="])
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  empty
    .assert()
    .failure()
    .stderr(predicate::str::contains("Required variable 'TASK_SECRET' is not set"));

  let mut non_interactive = Command::cargo_bin("octa")?;
  non_interactive
    .current_dir(tmp_dir.path())
    .args(["select-environment", "--non-interactive"])
    .env_remove("ENVIRONMENT")
    .env("GLOBAL_REQUIRED", "present")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  non_interactive.assert().failure().stderr(predicate::str::contains(
    "Interactive input is unavailable for required variable 'ENVIRONMENT'",
  ));

  let mut selected = Command::cargo_bin("octa")?;
  selected
    .current_dir(tmp_dir.path())
    .args(["select-environment", "ENVIRONMENT=production"])
    .env("GLOBAL_REQUIRED", "present")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  selected
    .assert()
    .success()
    .stdout(predicate::str::contains("production"));

  let mut invalid = Command::cargo_bin("octa")?;
  invalid
    .current_dir(tmp_dir.path())
    .args(["select-environment", "ENVIRONMENT=testing"])
    .env("GLOBAL_REQUIRED", "present")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  invalid
    .assert()
    .failure()
    .stderr(predicate::str::contains("must be one of: development, production"));

  let mut dynamic = Command::cargo_bin("octa")?;
  dynamic
    .current_dir(tmp_dir.path())
    .args(["select-dynamic-environment", "ENVIRONMENT=production"])
    .env("GLOBAL_REQUIRED", "present")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  dynamic
    .assert()
    .success()
    .stdout(predicate::str::contains("production"));

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
          shell: echo $greeting

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
          shell: "echo $VAR1"

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
  let read_command = "value=$(<value.txt); echo $value";
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
      FROM_PLUGIN_FUNCTION: '{{{{ plugin(key="tpl", value="from-function") }}}}'
      FROM_PLUGIN_FILTER: '{{{{ "from-filter" | plugin(key="tpl") }}}}'
    tpl: "$FROM_VAR|$FROM_SHELL|$FROM_FILTER|$FROM_PLUGIN_FUNCTION|$FROM_PLUGIN_FILTER"
"#
  );
  fs::write(tmp_dir.path().join("octafile.yml"), octafile)?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("show");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd.assert().success().stdout(predicate::str::contains(
    "dynamic|from-dotenv|dynamic|from-function|from-filter",
  ));

  Ok(())
}

#[test]
fn test_template_plugin_values_are_validated_at_runtime() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  show:
    env:
      INVALID: '{{ 42 | plugin(key="tpl") }}'
    tpl: "$INVALID"
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd.current_dir(tmp_dir.path());
  cmd.arg("show");
  cmd.env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd
    .assert()
    .failure()
    .stderr(predicate::str::contains("Invalid parameters for plugin 'tpl'"));

  Ok(())
}

#[test]
fn test_plugin_helpers_are_available_in_runtime_task_templates() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("octafile.yml"),
    r#"
version: 1
tasks:
  show:
    dir: '{{ plugin(key="tpl", value="nested") }}'
    preconditions:
      - '{{ plugin(key="tpl", value="true") }}'
    shell: echo runtime-templates
"#,
  )?;

  let mut cmd = Command::cargo_bin("octa")?;
  cmd
    .current_dir(tmp_dir.path())
    .arg("show")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  cmd
    .assert()
    .success()
    .stdout(predicate::str::contains("runtime-templates"));
  assert!(tmp_dir.path().join("nested").is_dir());

  Ok(())
}

#[test]
fn test_shell_environment_is_evaluated_once_per_execution() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::create_dir(tmp_dir.path().join("nested"))?;
  let env_command = "echo call >> ../calls.txt; echo value";
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
    .stdout(predicate::str::contains("[task1] task1"))
    .stdout(predicate::str::contains("[task2] task2"))
    .stdout(predicate::str::contains("[task3] task3"));

  Ok(())
}

#[test]
fn test_adaptive_output_uses_built_graph_parallelism() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1
tasks:
  build:
    execute_mode: parallel
    cmds:
      - shell: echo first
      - shell: echo second
"#,
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .env("OCTA_TESTS", "")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .arg("build");
  command
    .assert()
    .success()
    .stdout(predicate::str::contains("[build] first"))
    .stdout(predicate::str::contains("[build] second"));

  let mut serialized = Command::cargo_bin("octa")?;
  serialized
    .current_dir(tmp_dir.path())
    .env("OCTA_TESTS", "")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .args(["--concurrency", "1", "build"]);
  serialized
    .assert()
    .success()
    .stdout(predicate::str::contains("[build] first").not())
    .stdout(predicate::str::contains("\nfirst\n"))
    .stdout(predicate::str::contains("\nsecond\n"));

  Ok(())
}

#[test]
fn test_grouped_output_is_flushed_by_completed_task() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1
output:
  group:
    begin: 'BEGIN {{.TASK}} {{.GROUP_LABEL}}'
    end: 'END {{.TASK}}'

vars:
  GROUP_LABEL: runtime

tasks:
  slow:
    shell: |
      echo slow-start
      touch slow-started
      while [ ! -f fast-finished ]; do sleep 0.01; done
      sleep 1
      echo slow-end
  fast:
    shell: |
      while [ ! -f slow-started ]; do sleep 0.01; done
      echo fast-output
      touch fast-finished
"#,
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .args(["--parallel", "--output", "group", "slow", "fast"])
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  let output = command.output()?;
  assert!(
    output.status.success(),
    "stdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let stdout = String::from_utf8(output.stdout)?;
  let task_output = stdout
    .lines()
    .filter(|line| matches!(*line, "slow-start" | "slow-end" | "fast-output"))
    .collect::<Vec<_>>();
  assert_eq!(task_output, ["fast-output", "slow-start", "slow-end"]);
  assert!(stdout.contains("BEGIN slow runtime"));
  assert!(stdout.contains("END slow"));
  assert!(stdout.contains("BEGIN fast runtime"));
  assert!(stdout.contains("END fast"));

  Ok(())
}

#[test]
fn test_prefixed_output_uses_task_name_and_custom_prefix() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  build: echo compiled
  deploy:
    prefix: '{{.TASK}}-{{.ENVIRONMENT}}'
    vars:
      ENVIRONMENT: production
    shell: echo released >&2
"#,
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .args(["--parallel", "--output", "prefixed", "build", "deploy"])
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());

  command
    .assert()
    .success()
    .stdout(predicate::str::contains("[build] compiled"))
    .stderr(predicate::str::contains("[deploy-production] released"));

  Ok(())
}

#[test]
fn test_task_specific_output_style_overrides_the_global_style() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1
output: interleaved

tasks:
  plain: echo plain-output
  labeled:
    presentation:
      output: prefix
    shell: echo labeled-output
"#,
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .args(["--parallel", "plain", "labeled"])
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
  command
    .assert()
    .success()
    .stdout(predicate::str::contains("plain-output"))
    .stdout(predicate::str::contains("[labeled] labeled-output"))
    .stdout(predicate::str::contains("[plain] plain-output").not());

  let mut forced = Command::cargo_bin("octa")?;
  forced
    .current_dir(tmp_dir.path())
    .args(["--parallel", "--output", "interleave", "labeled"])
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("forced-cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .assert()
    .success()
    .stdout(predicate::str::contains("labeled-output"))
    .stdout(predicate::str::contains("[labeled] labeled-output").not());

  Ok(())
}

#[test]
fn test_timed_output_reveals_a_long_lived_status_line() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1
output: timed

tasks:
  build:
    shell: |
      echo compiling
      sleep 1.2
"#,
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .arg("build")
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .assert()
    .success()
    .stdout(predicate::str::contains("[build] compiling"));

  Ok(())
}

#[test]
fn test_octafile_output_and_environment_json_override() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1
output: prefixed

tasks:
  build: echo configured-output
"#,
  )?;

  let command = || -> Result<Command, Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .arg("build");
    Ok(command)
  };

  command()?
    .assert()
    .success()
    .stdout(predicate::str::contains("[build] configured-output"));

  let output = command()?.env("TASK_OUTPUT", "json").output()?;
  assert!(output.status.success());
  let entries = String::from_utf8(output.stdout)?
    .lines()
    .map(serde_json::from_str::<serde_json::Value>)
    .collect::<Result<Vec<_>, _>>()?;
  assert!(entries.iter().all(|entry| entry["schema_version"] == 1));
  assert_eq!(
    entries
      .iter()
      .map(|entry| entry["sequence"].as_u64().unwrap())
      .collect::<Vec<_>>(),
    (1..=entries.len() as u64).collect::<Vec<_>>()
  );
  assert!(entries.iter().any(|entry| {
    entry["category"] == "execution"
      && entry["data"]["type"] == "output"
      && entry["data"]["payload"]["data"] == "configured-output"
  }));

  Ok(())
}

#[test]
fn test_json_output_rejects_raw_mode_before_execution() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  build: echo should-not-run\n",
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .args(["--output", "json", "--raw", "build"])
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .assert()
    .failure()
    .stderr(predicate::str::contains(
      "raw/PTY mode cannot be combined with JSON output",
    ))
    .stdout(predicate::str::contains("should-not-run").not());

  Ok(())
}

#[test]
fn test_raw_mode_runs_through_the_pty_without_a_terminal() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  interactive: echo raw-output\n",
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .args(["--raw", "interactive"])
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .assert()
    .success()
    .stdout(predicate::str::contains("raw-output"));

  Ok(())
}

#[cfg(unix)]
#[test]
fn test_raw_mode_uses_and_restores_an_attached_terminal() -> Result<(), Box<dyn std::error::Error>> {
  use std::io::Read;

  use portable_pty::{native_pty_system, CommandBuilder, PtySize};

  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  interactive: echo terminal-output\n",
  )?;
  let pair = native_pty_system().openpty(PtySize {
    rows: 24,
    cols: 80,
    pixel_width: 0,
    pixel_height: 0,
  })?;
  let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_octa"));
  command.cwd(tmp_dir.path());
  command.args(["--raw", "interactive"]);
  command.env("TERM", "xterm-256color");
  command.env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"));
  command.env("OCTA_PLUGINS_DIR", validation_plugins_dir());

  let mut reader = pair.master.try_clone_reader()?;
  let output = std::thread::spawn(move || {
    let mut output = String::new();
    reader.read_to_string(&mut output).unwrap();
    output
  });
  let mut child = pair.slave.spawn_command(command)?;
  drop(pair.slave);
  let status = child.wait()?;
  drop(pair.master);
  let output = output.join().unwrap();

  assert!(status.success(), "{output}");
  assert!(output.contains("terminal-output"), "{output:?}");
  Ok(())
}

#[test]
fn test_keep_order_streams_and_replays_in_declaration_order() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1
output: keep-order

tasks:
  slow:
    shell: |
      echo slow-start
      touch slow-started
      while [ ! -f fast-finished ]; do sleep 0.01; done
      echo slow-end
  fast:
    shell: |
      while [ ! -f slow-started ]; do sleep 0.01; done
      echo fast-output
      touch fast-finished
"#,
  )?;

  let mut command = Command::cargo_bin("octa")?;
  let output = command
    .current_dir(tmp_dir.path())
    .args(["--parallel", "slow", "fast"])
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .output()?;
  assert!(output.status.success());

  let stdout = String::from_utf8(output.stdout)?;
  let task_output = stdout
    .lines()
    .filter(|line| line.contains("slow-start") || line.contains("slow-end") || line.contains("fast-output"))
    .collect::<Vec<_>>();
  assert_eq!(
    task_output,
    ["[slow] slow-start", "[slow] slow-end", "[fast] fast-output"]
  );

  Ok(())
}

#[test]
fn test_silent_can_hide_only_one_task_stream() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  stdout-hidden:
    silent: stdout
    shell: echo hidden-stdout; echo visible-stderr >&2
  stderr-hidden:
    silent: stderr
    shell: echo visible-stdout; echo hidden-stderr >&2
"#,
  )?;

  let run = |task: &str| -> Result<std::process::Output, Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    Ok(
      command
        .current_dir(tmp_dir.path())
        .arg(task)
        .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
        .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
        .output()?,
    )
  };

  let stdout_hidden = run("stdout-hidden")?;
  assert!(stdout_hidden.status.success());
  assert!(!String::from_utf8(stdout_hidden.stdout)?.contains("hidden-stdout"));
  assert!(String::from_utf8(stdout_hidden.stderr)?.contains("visible-stderr"));

  let stderr_hidden = run("stderr-hidden")?;
  assert!(stderr_hidden.status.success());
  assert!(String::from_utf8(stderr_hidden.stdout)?.contains("visible-stdout"));
  assert!(!String::from_utf8(stderr_hidden.stderr)?.contains("hidden-stderr"));

  Ok(())
}

#[test]
fn test_on_error_output_discards_success_and_replays_failure() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1

tasks:
  success: echo hidden-output
  failure: echo visible-output && exit 1
"#,
  )?;

  let run = |task: &str| -> Result<std::process::Output, Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    let output = command
      .current_dir(tmp_dir.path())
      .args(["--output", "on-error", "--ci", "none", task])
      .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
      .output()?;
    Ok(output)
  };

  let success = run("success")?;
  assert!(success.status.success());
  assert!(!String::from_utf8(success.stdout)?.contains("hidden-output"));

  let failure = run("failure")?;
  assert!(!failure.status.success());
  assert!(String::from_utf8(failure.stdout)?.contains("visible-output"));

  Ok(())
}

#[test]
fn test_github_actions_detection_emits_an_annotation_and_can_be_disabled() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    "version: 1\ntasks:\n  failure: exit 1\n",
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .arg("failure")
    .env("GITHUB_ACTIONS", "true")
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());

  command
    .assert()
    .failure()
    .stdout(predicate::str::contains("::error title=Task 'failure' failed::"));

  let mut disabled = Command::cargo_bin("octa")?;
  disabled
    .current_dir(tmp_dir.path())
    .args(["--ci", "none", "failure"])
    .env("GITHUB_ACTIONS", "true")
    .env("OCTA_CACHE_DIR", tmp_dir.path().join("cache"))
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir());

  disabled
    .assert()
    .failure()
    .stdout(predicate::str::contains("::error").not());

  Ok(())
}

#[test]
fn test_octafile_concurrency_limits_parallel_commands() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let delay = "end=$((SECONDS + 1)); while (( SECONDS < end )); do :; done";
  let commands = [
    format!("echo A-start>>trace.txt && {delay} && echo A-end>>trace.txt"),
    format!("echo B-start>>trace.txt && {delay} && echo B-end>>trace.txt"),
  ];
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    format!(
      r#"
version: 1
concurrency: 1

tasks:
  limited:
    cmds:
      - shell: "{first}"
      - shell: "{second}"
"#,
      first = commands[0],
      second = commands[1],
    ),
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .env("OCTA_TESTS", "")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .args(["--parallel", "limited"]);
  command.assert().success();

  let trace = fs::read_to_string(tmp_dir.path().join("trace.txt"))?;
  let lines = trace.lines().map(str::trim).collect::<Vec<_>>();
  assert!(
    lines == ["A-start", "A-end", "B-start", "B-end"] || lines == ["B-start", "B-end", "A-start", "A-end"],
    "commands overlapped: {lines:?}"
  );

  Ok(())
}

#[test]
fn test_parallel_commands_share_one_plugin_connection() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  let delay = "end=$((SECONDS + 1)); while (( SECONDS < end )); do :; done";
  let commands = [
    format!("echo A-start>>trace.txt && {delay} && echo A-end>>trace.txt"),
    format!("echo B-start>>trace.txt && {delay} && echo B-end>>trace.txt"),
  ];
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    format!(
      r#"
version: 1

tasks:
  parallel:
    execute_mode: parallel
    cmds:
      - shell: "{first}"
      - shell: "{second}"
"#,
      first = commands[0],
      second = commands[1],
    ),
  )?;

  let mut command = Command::cargo_bin("octa")?;
  command
    .current_dir(tmp_dir.path())
    .env("OCTA_TESTS", "")
    .env("OCTA_PLUGINS_DIR", validation_plugins_dir())
    .arg("parallel");
  command.assert().success();

  let trace = fs::read_to_string(tmp_dir.path().join("trace.txt"))?;
  let lines = trace.lines().map(str::trim).collect::<Vec<_>>();
  assert_eq!(lines.len(), 4);
  assert!(lines[..2].iter().all(|line| line.ends_with("-start")), "{lines:?}");
  assert!(lines[2..].iter().all(|line| line.ends_with("-end")), "{lines:?}");

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
fn test_internal_include_hides_tasks_but_allows_task_references() -> Result<(), Box<dyn std::error::Error>> {
  let tmp_dir = TempDir::new()?;
  fs::write(
    tmp_dir.path().join("Octafile.yml"),
    r#"
version: 1
includes:
  helpers:
    octafile: Helpers.yml
    internal: true
tasks:
  build:
    cmds:
      - task: helpers:prepare
      - echo build
"#,
  )?;
  fs::write(
    tmp_dir.path().join("Helpers.yml"),
    r#"
version: 1
tasks:
  prepare: echo prepare
"#,
  )?;

  let command = || -> Result<Command, Box<dyn std::error::Error>> {
    let mut command = Command::cargo_bin("octa")?;
    command
      .current_dir(tmp_dir.path())
      .env("OCTA_PLUGINS_DIR", validation_plugins_dir());
    Ok(command)
  };

  command()?
    .arg("--list-tasks")
    .assert()
    .success()
    .stdout(predicate::str::contains("build"))
    .stdout(predicate::str::contains("helpers:prepare").not());
  command()?
    .arg("helpers:prepare")
    .assert()
    .failure()
    .stderr(predicate::str::contains("Command not found: helpers:prepare"));
  command()?
    .arg("build")
    .assert()
    .success()
    .stdout(predicate::str::contains("prepare"))
    .stdout(predicate::str::contains("build"));

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
