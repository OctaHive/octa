use super::*;

use octa_dag::Identifiable;
use octa_octafile::Octafile;
use std::{fs, io, sync::Mutex as StdMutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

use crate::vars::VariablePrompt;
use octa_output::{Console, ConsoleRecord, ConsoleRenderer, ExecutionEvent};

struct TestVariableResolver;

struct TestSourceStrategy;

#[async_trait::async_trait]
impl SourceStrategy for TestSourceStrategy {
  fn key(&self) -> &'static str {
    "test"
  }

  async fn fingerprint(&self, _sources: &[PathBuf], _cancel_token: &CancellationToken) -> ExecutorResult<Vec<u8>> {
    Ok(vec![1])
  }
}

#[async_trait::async_trait]
impl VariableResolver for TestVariableResolver {
  async fn resolve(&self, _prompt: &VariablePrompt) -> Result<String, String> {
    Ok("value".to_owned())
  }
}

fn create_test_task() -> Task {
  Task {
    plugin: Some(PluginCommand {
      key: "shell".to_string(),
      value: serde_yml::Value::String("echo test".to_string()),
    }),
    ..Task::default()
  }
}

#[test]
fn repeated_dependencies_receive_distinct_invocation_names() {
  let dependencies = vec![Deps::from("build".to_owned()), Deps::from("build".to_owned())];
  let mut frequencies = TaskGraphBuilder::build_deps_frequency_map(&dependencies);

  assert_eq!(
    TaskGraphBuilder::generate_unique_task_name("build", &mut frequencies),
    "build_1"
  );
  assert_eq!(
    TaskGraphBuilder::generate_unique_task_name("build", &mut frequencies),
    "build_2"
  );
}

#[test]
fn test_matches_platform_and_architecture() {
  for selector in ["linux", "x86_64", "amd64", "x64", "linux/x86_64", "linux/amd64"] {
    assert!(platform::matches(selector, "linux", "x86_64"), "{selector} must match");
  }

  for selector in ["windows", "arm64", "linux/arm64", "windows/amd64"] {
    assert!(
      !platform::matches(selector, "linux", "x86_64"),
      "{selector} must not match"
    );
  }

  assert!(platform::matches(" macOS / AARCH64 ", "macos", "arm64"));
  assert!(platform::matches("darwin/amd64", "macos", "x86_64"));
}

#[tokio::test]
async fn test_task_graph_builder_new() -> ExecutorResult<()> {
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?
    .with_variable_resolver(Arc::new(TestVariableResolver))
    .with_source_strategy(SourceMethod::Hash, TestSourceStrategy);
  assert!(builder.command_args.is_empty());
  assert!(builder.variable_overrides.is_empty());
  assert!(builder.variable_resolver.is_some());
  assert_eq!(
    builder
      .variable_resolver
      .as_ref()
      .unwrap()
      .resolve(&VariablePrompt {
        name: "TEST".to_owned(),
        question: "Test".to_owned(),
        enum_values: None,
        secret: false,
      })
      .await,
    Ok("value".to_owned())
  );
  assert_eq!(builder.source_strategies.resolve(&SourceMethod::Hash)?.key(), "test");
  assert!(builder.dir.exists());
  Ok(())
}

#[tokio::test]
async fn test_build_simple_task() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        test:
          shell: echo "test"
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;
  let dag = builder.build(octafile, "test", true, vec![]).await?;

  assert_eq!(dag.node_count(), 1);
  assert!(!dag.has_cycle()?);
  let tasks: Vec<String> = dag.nodes().iter().map(|n| n.name.clone()).collect();

  assert!(tasks.contains(&"test".to_owned()));
  Ok(())
}

#[tokio::test]
async fn interactive_task_body_shares_one_raw_exclusive_session() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        terminal:
          interactive: true
          cmds:
            - echo one
            - echo two
  "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let plan = TaskGraphBuilder::new(plugin_manager)?
    .build(octafile, "terminal", false, vec![])
    .await?;

  let commands = plan
    .nodes()
    .iter()
    .filter(|node| !node.is_internal())
    .collect::<Vec<_>>();
  assert_eq!(commands.len(), 2);
  assert!(commands.iter().all(|node| node.raw));
  let session = commands[0].interactive_session().unwrap();
  assert!(commands.iter().all(|node| node.interactive_session() == Some(session)));
  Ok(())
}

#[tokio::test]
async fn command_nodes_share_an_ordered_invocation_scope() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        build:
          prefix: compiler
          cmds:
            - echo one
            - echo two
        test: echo test
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let allocator = Arc::new(ConsoleScopeAllocator::default());
  let build = TaskGraphBuilder::new(plugin_manager.clone())?
    .with_scope_allocator(allocator.clone())
    .build(octafile.clone(), "build", false, vec![])
    .await?;
  let test = TaskGraphBuilder::new(plugin_manager)?
    .with_scope_allocator(allocator)
    .build(octafile, "test", false, vec![])
    .await?;

  let build_scopes = build
    .nodes()
    .iter()
    .filter_map(|node| node.output_scope())
    .collect::<Vec<_>>();
  let test_scope = test.nodes().iter().next().unwrap().output_scope().unwrap();
  assert_eq!(build_scopes.len(), 2);
  assert!(build_scopes.iter().all(|scope| scope.id() == 0));
  assert!(build_scopes.iter().all(|scope| scope.label() == "build"));
  assert!(build_scopes.iter().all(|scope| scope.prefix() == "compiler"));
  assert_eq!(test_scope.id(), 1);
  assert_eq!(test_scope.label(), "test");
  assert_eq!(test_scope.prefix(), "test");

  Ok(())
}

#[derive(Clone)]
struct RecordingRenderer(Arc<StdMutex<Vec<ConsoleRecord>>>);

impl ConsoleRenderer for RecordingRenderer {
  fn render(&mut self, entry: &octa_output::ConsoleEntry) -> io::Result<()> {
    self.0.lock().unwrap().push(entry.record().clone());
    Ok(())
  }
}

#[tokio::test]
async fn nested_and_deferred_invocations_close_every_declared_scope() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  fs::write(
    temp_dir.path().join("Octafile.yml"),
    r#"
version: 1
tasks:
  cleanup: echo cleanup
  child: echo child
  parent:
    cmds:
      - task: child
      - defer:
          task: cleanup
"#,
  )?;
  let octafile = Octafile::load(
    Some(temp_dir.path().join("Octafile.yml")),
    false,
    vec!["shell".to_owned()],
    "shell",
  )?;
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{project_root}/../../plugins")));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();
  let allocator = Arc::new(ConsoleScopeAllocator::default());
  let events = Arc::new(StdMutex::new(Vec::new()));
  let console = Arc::new(Console::new(RecordingRenderer(events.clone())));
  let plan = TaskGraphBuilder::new(plugin_manager.clone())?
    .with_scope_allocator(allocator)
    .build(octafile, "parent", false, Vec::new())
    .await?;
  let executor = Executor::new(
    plugin_manager.clone(),
    plan,
    crate::executor::ExecutorConfig {
      emit_run_events: true,
      console,
      ..crate::executor::ExecutorConfig::default()
    },
    None,
    Arc::new(sled::Config::new().temporary(true).open().unwrap()),
    false,
    false,
    None,
  )?;

  executor.execute(CancellationToken::new(), "parent").await?;

  {
    let events = events.lock().unwrap();
    let declared = events
      .iter()
      .filter_map(|event| match event {
        ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { scope, .. }) => {
          Some((scope.id(), scope.label().to_owned()))
        },
        _ => None,
      })
      .collect::<Vec<_>>();
    let started = events
      .iter()
      .filter_map(|event| match event {
        ConsoleRecord::Execution(ExecutionEvent::ScopeStarted { scope, .. }) => {
          Some((scope.id(), scope.label().to_owned()))
        },
        _ => None,
      })
      .collect::<Vec<_>>();
    let mut finished = events
      .iter()
      .filter_map(|event| match event {
        ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, .. }) => Some(scope.id()),
        _ => None,
      })
      .collect::<Vec<_>>();
    let mut declared_ids = declared.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    declared_ids.sort_unstable();
    finished.sort_unstable();
    assert_eq!(finished, declared_ids);
    assert!(started.iter().all(|entry| declared.contains(entry)));
    assert!(started.iter().any(|(_, label)| label == "parent"));
    assert!(started.iter().any(|(_, label)| label == "child"));
    assert!(started.iter().any(|(_, label)| label == "cleanup"));
  }
  plugin_manager.shutdown_all().await;

  Ok(())
}

#[tokio::test]
async fn test_command_timeout_inherits_and_overrides_task_timeout() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        called:
          timeout: 20s
          shell: echo called
        pipeline:
          timeout: 10s
          cmds:
            - echo inherited
            - task: called
              timeout: 2s
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let dag = TaskGraphBuilder::new(plugin_manager)?
    .build(octafile, "pipeline", false, vec![])
    .await?;

  let inherited = dag.nodes().iter().find(|task| task.name == "echo inherited").unwrap();
  let overridden = dag.nodes().iter().find(|task| task.name == "called").unwrap();
  assert_eq!(
    inherited.timeout.unwrap().duration(),
    std::time::Duration::from_secs(10)
  );
  assert_eq!(
    overridden.timeout.unwrap().duration(),
    std::time::Duration::from_secs(2)
  );

  Ok(())
}

#[tokio::test]
async fn test_command_options_inherit_and_override_task_defaults() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        called:
          if: referenced-condition
          silent: true
          ignore_error: true
          shell: echo called
        pipeline:
          if: containing-condition
          silent: true
          ignore_error: true
          cmds:
            - shell: echo inherited
            - shell: echo overridden
              if: command-condition
              silent: false
              ignore_error: false
            - task: called
              if: reference-condition
              silent: false
              ignore_error: false
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let dag = TaskGraphBuilder::new(plugin_manager)?
    .build(octafile, "pipeline", false, vec![])
    .await?;

  let inherited = dag.nodes().iter().find(|task| task.name == "echo inherited").unwrap();
  assert!(inherited.conditions().is_empty());
  assert_eq!(inherited.silence, octa_octafile::Silence::All);
  assert!(inherited.ignore_errors);

  let overridden = dag.nodes().iter().find(|task| task.name == "echo overridden").unwrap();
  assert_eq!(overridden.conditions(), ["command-condition"]);
  assert_eq!(overridden.silence, octa_octafile::Silence::None);
  assert!(!overridden.ignore_errors);

  let referenced = dag.nodes().iter().find(|task| task.name == "called").unwrap();
  assert!(referenced.conditions().is_empty());
  assert_eq!(referenced.silence, octa_octafile::Silence::None);
  assert!(!referenced.ignore_errors);

  let gates = dag
    .nodes()
    .iter()
    .filter(|task| task.name.contains("condition for"))
    .map(|task| {
      assert!(crate::task::TaskItem::requires_concurrency_permit(task.as_ref()));
      task.conditions()
    })
    .collect::<Vec<_>>();
  assert!(gates.contains(&vec!["containing-condition".to_string()]));
  assert!(gates.contains(&vec!["referenced-condition".to_string()]));
  assert!(gates.contains(&vec!["reference-condition".to_string()]));

  Ok(())
}

#[tokio::test]
async fn test_failfast_inherits_and_overrides_octafile_default() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      failfast: true
      tasks:
        inherited:
          sources: [Octafile.yml]
          shell: echo inherited
        overridden:
          failfast: false
          sources: [Octafile.yml]
          shell: echo overridden
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let dag = TaskGraphBuilder::new(plugin_manager)?
    .build(octafile, "**", true, vec![])
    .await?;

  let inherited = dag.nodes().iter().find(|task| task.name == "inherited").unwrap();
  let overridden = dag.nodes().iter().find(|task| task.name == "overridden").unwrap();
  assert!(inherited.failfast);
  assert!(!overridden.failfast);

  for phase in ["Check freshness", "Commit freshness"] {
    let inherited = dag
      .nodes()
      .iter()
      .find(|task| task.name == format!("{phase} for inherited"))
      .unwrap();
    let overridden = dag
      .nodes()
      .iter()
      .find(|task| task.name == format!("{phase} for overridden"))
      .unwrap();
    assert!(inherited.failfast);
    assert!(!overridden.failfast);
  }

  Ok(())
}

#[tokio::test]
async fn test_failfast_is_inherited_by_task_references_and_dependencies() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        pipeline:
          failfast: true
          execute_mode: parallel
          deps:
            - dependency
          cmds:
            - task: called
        dependency:
          failfast: false
          shell: echo dependency
        called:
          failfast: false
          shell: echo called
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let dag = TaskGraphBuilder::new(plugin_manager)?
    .build(octafile, "pipeline", true, vec![])
    .await?;

  let dependency = dag.nodes().iter().find(|task| task.name == "dependency").unwrap();
  let called = dag.nodes().iter().find(|task| task.name == "called").unwrap();
  assert!(dependency.failfast);
  assert!(called.failfast);

  Ok(())
}

#[tokio::test]
async fn test_octafile_defaults_are_inherited_and_can_be_overridden() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      run: once
      source_strategy: hash
      includes:
        child: child.yml
      tasks:
        inherited:
          sources: [Octafile.yml]
          shell: echo inherited
        overridden:
          run: changed
          source_strategy: timestamp
          sources: [Octafile.yml]
          shell: echo overridden
    "#;
  let child_content = r#"
      version: 1
      run: changed
      source_strategy: timestamp
      tasks:
        child_inherited:
          sources: [child.yml]
          shell: echo child
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;
  fs::write(temp_dir.path().join("child.yml"), child_content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?.with_source_strategy(SourceMethod::Hash, TestSourceStrategy);
  let dag = builder.build(octafile, "**", true, vec![]).await?;

  let inherited = dag.nodes().iter().find(|task| task.name == "inherited").unwrap();
  let overridden = dag.nodes().iter().find(|task| task.name == "overridden").unwrap();
  let child = dag
    .nodes()
    .iter()
    .find(|task| task.name == "child:child_inherited")
    .unwrap();
  assert_eq!(inherited.run_mode, task::RunMode::Once);
  assert_eq!(overridden.run_mode, task::RunMode::Changed);
  assert_eq!(child.run_mode, task::RunMode::Changed);
  let inherited_gate = dag
    .nodes()
    .iter()
    .find(|task| task.name == "Check freshness for inherited")
    .unwrap();
  let overridden_gate = dag
    .nodes()
    .iter()
    .find(|task| task.name == "Check freshness for overridden")
    .unwrap();
  assert_eq!(inherited_gate.source_method(), Some(SourceMethod::Hash));
  assert_eq!(inherited_gate.source_strategy_key(), Some("test"));
  assert_eq!(overridden_gate.source_method(), Some(SourceMethod::Timestamp));
  assert_eq!(overridden_gate.source_strategy_key(), Some("timestamp"));

  Ok(())
}

#[tokio::test]
async fn custom_source_strategy_can_be_selected_from_octafile() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(
    &octafile_path,
    r#"
version: 1
source_strategy: content-addressed
tasks:
  build:
    sources: [Octafile.yml]
    shell: echo build
"#,
  )?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_owned()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let method = SourceMethod::custom("content-addressed");
  let dag = TaskGraphBuilder::new(plugin_manager)?
    .with_source_strategy(method, TestSourceStrategy)
    .build(octafile, "build", false, vec![])
    .await?;
  let gate = dag
    .nodes()
    .iter()
    .find(|task| task.name == "Check freshness for build")
    .unwrap();

  assert_eq!(gate.source_method(), Some(SourceMethod::custom("content-addressed")));
  assert_eq!(gate.source_strategy_key(), Some("test"));
  Ok(())
}

#[tokio::test]
async fn test_build_with_dependencies() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        task1:
          shell: echo "task1"
        task2:
          shell: echo "task2"
          deps:
            - task1
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;
  let dag = builder.build(octafile, "task2", true, vec![]).await?;

  let id_to_name: HashMap<String, String> = dag
    .nodes()
    .iter()
    .map(|item| (item.name.clone(), item.id.clone()))
    .collect();

  assert_eq!(dag.node_count(), 3);
  assert!(!dag.has_cycle()?);
  let tasks: Vec<String> = dag.nodes().iter().map(|n| n.name.clone()).collect();
  assert!(tasks.contains(&"task1".to_owned()));
  assert!(tasks.contains(&"task2".to_owned()));

  assert!(dag.edges().contains_key(&id_to_name["task1"]));

  Ok(())
}

#[tokio::test]
async fn test_command_not_found() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        test:
          shell: echo "test"
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;
  let result = builder.build(octafile, "nonexistent", true, vec![]).await;

  assert!(matches!(result, Err(ExecutorError::CommandNotFound(_))));
  Ok(())
}

#[tokio::test]
async fn test_platform_specific_tasks() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        test_macos:
          shell: echo "test"
          platforms:
            - macos
        test_linux:
          shell: echo "test"
          platforms:
            - linux
        test_windows:
          shell: echo "test"
          platforms:
            - windows
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;

  let dag = if cfg!(target_os = "linux") {
    builder.build(octafile, "test_linux", true, vec![]).await?
  } else if cfg!(target_os = "windows") {
    builder.build(octafile, "test_windows", true, vec![]).await?
  } else {
    builder.build(octafile, "test_macos", true, vec![]).await?
  };

  // The number of nodes will depend on the current platform
  assert!(!dag.has_cycle()?);
  Ok(())
}

#[tokio::test]
async fn test_platform_mismatch_creates_skipped_plan() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        test:
          platforms: [unsupported]
          shell: echo test
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;
  let dag = builder.build(octafile, "test", true, vec![]).await?;

  assert_eq!(dag.node_count(), 1);
  assert!(dag.nodes().iter().all(|task| task.is_internal()));
  Ok(())
}

#[tokio::test]
async fn test_command_with_args() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      tasks:
        test:
          shell: echo "{{ COMMAND_ARGS }}"
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;
  let args = vec!["arg1".to_string(), "arg2".to_string()];
  let dag = builder.build(octafile, "test", true, args).await?;

  assert_eq!(dag.node_count(), 1);
  Ok(())
}

#[tokio::test]
async fn test_variable_inheritance() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      vars:
        GLOBAL: "global"
      tasks:
        test:
          vars:
            LOCAL: "local"
          shell: echo "{{ GLOBAL }} {{ LOCAL }}"
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?.with_variable_overrides(vec![
    ("GLOBAL".to_owned(), "override".to_owned()),
    ("COMBINED".to_owned(), "{{ LOCAL }}-override".to_owned()),
  ]);
  let dag = builder.build(octafile, "test", true, vec![]).await?;

  assert_eq!(dag.node_count(), 1);
  let task = dag.nodes().iter().find(|task| task.name == "test").unwrap();
  let mut vars = task.vars.clone();
  vars.expand(true).await?;
  assert_eq!(vars.get("GLOBAL").and_then(|value| value.as_str()), Some("override"));
  assert_eq!(vars.get("LOCAL").and_then(|value| value.as_str()), Some("local"));
  assert_eq!(
    vars.get("COMBINED").and_then(|value| value.as_str()),
    Some("local-override")
  );
  Ok(())
}

#[tokio::test]
async fn test_dotenv_layers_and_search() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let nested_dir = temp_dir.path().join("work").join("nested");
  fs::create_dir_all(temp_dir.path().join("config"))?;
  fs::create_dir_all(&nested_dir)?;
  fs::write(
    temp_dir.path().join("Octafile.yml"),
    r#"
        version: 1
        vars:
          PROFILE: local
        dotenv:
          - .env.{{ PROFILE }}
          - config/base.env
        env:
          EXPLICIT_ROOT: root
        tasks:
          test:
            dir: work/nested
            dotenv:
              - .env.task
            env:
              TASK_VALUE: explicit
            shell: echo test
      "#,
  )?;
  fs::write(
    temp_dir.path().join(".env.local"),
    "ROOT_PRIORITY=first\nROOT_DOTENV=loaded\n",
  )?;
  fs::write(
    temp_dir.path().join("config").join("base.env"),
    "ROOT_PRIORITY=second\n",
  )?;
  fs::write(
    temp_dir.path().join("work").join(".env.task"),
    "TASK_DOTENV=searched\nTASK_VALUE=dotenv\n",
  )?;

  let octafile = Octafile::load(
    Some(temp_dir.path().join("Octafile.yml")),
    false,
    vec!["shell".to_string()],
    "shell",
  )?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let mut builder = TaskGraphBuilder::new(plugin_manager)?;
  builder.dir = temp_dir.path().to_path_buf();
  let command = builder.find_and_filter_commands(&octafile, "test")?.remove(0);
  let mut vars = builder.collect_vars_with_identity(&command, None)?.runtime;
  vars.expand(false).await?;
  let plan = builder
    .collect_environment_plan(
      &command,
      Some(HashMap::from([(
        "TASK_VALUE".to_owned(),
        octa_octafile::EnvValue::String("execute".to_owned()),
      )])),
    )
    .unwrap();
  let envs = plan.resolve(&vars, None, false, CancellationToken::new()).await?;

  assert_eq!(envs.get("ROOT_PRIORITY"), Some(&"first".to_string()));
  assert_eq!(envs.get("ROOT_DOTENV"), Some(&"loaded".to_string()));
  assert_eq!(envs.get("EXPLICIT_ROOT"), Some(&"root".to_string()));
  assert_eq!(envs.get("TASK_DOTENV"), Some(&"searched".to_string()));
  assert_eq!(envs.get("TASK_VALUE"), Some(&"execute".to_string()));
  Ok(())
}

#[tokio::test]
async fn test_environment_inheritance() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let content = r#"
      version: 1
      env:
        GLOBAL_ENV: "global"
      tasks:
        test:
          env:
            LOCAL_ENV: "local"
          shell: echo "test"
    "#;
  let octafile_path = temp_dir.path().join("Octafile.yml");
  fs::write(&octafile_path, content)?;

  let octafile = Octafile::load(Some(octafile_path), false, vec!["shell".to_string()], "shell")?;
  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;
  let dag = builder.build(octafile, "test", true, vec![]).await?;

  assert_eq!(dag.node_count(), 1);
  let task = dag.nodes().iter().find(|task| task.name == "test").unwrap();
  let mut envs = task.envs.clone();
  envs.expand().await?;
  assert_eq!(envs.get("GLOBAL_ENV"), Some(&"global".to_string()));
  assert_eq!(envs.get("LOCAL_ENV"), Some(&"local".to_string()));
  Ok(())
}

#[tokio::test]
async fn test_process_hierarchy_vars() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let root_octafile = setup_test_octafiles(&temp_dir).await?;

  let nested_octafile = root_octafile.get_included("nested")?.unwrap();
  let deep_octafile = nested_octafile.get_included("deep")?.unwrap();

  let cmd = FindResult {
    name: "test_cmd".to_string(),
    octafile: deep_octafile,
    task: create_test_task(),
  };

  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;
  let mut vars = Vars::new();
  builder.process_hierarchy_vars(&cmd, &mut vars)?;

  vars.expand(false).await?;

  // Updated assertions for Tera values
  assert_eq!(vars.get("NESTED_VAR").and_then(|v| v.as_str()), Some("nested_value"));
  assert_eq!(vars.get("DEEP_VAR").and_then(|v| v.as_str()), Some("deep_value"));
  assert!(vars.get("TASKFILE_DIR").is_some());

  Ok(())
}

#[tokio::test]
async fn test_nested_includes() -> ExecutorResult<()> {
  let temp_dir = TempDir::new().unwrap();
  let root_octafile = setup_test_octafiles(&temp_dir).await?;

  let plugins_dir = PathBuf::from("../../plugins/test.py").canonicalize().unwrap();
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));
  let builder = TaskGraphBuilder::new(plugin_manager)?;
  let dag = builder.build(root_octafile, "**:deep_task", true, vec![]).await?;

  assert!(dag.node_count() > 0);
  assert!(!dag.has_cycle()?);
  Ok(())
}

async fn setup_test_octafiles(temp_dir: &TempDir) -> ExecutorResult<Arc<Octafile>> {
  // Create root octafile content
  let root_content = r#"
      version: 1
      vars:
        ROOT_VAR: "root_value"
      includes:
        nested:
          octafile: nested/Octafile.yml
      tasks:
        root_task:
          shell: echo "root"
    "#;

  // Create nested octafile content
  let nested_content = r#"
      version: 1
      vars:
        NESTED_VAR: "nested_value"
      includes:
        deep:
          octafile: deep/Octafile.yml
      tasks:
        nested_task:
          shell: echo "nested"
    "#;

  // Create deep octafile content
  let deep_content = r#"
      version: 1
      vars:
        DEEP_VAR: "deep_value"
      tasks:
        deep_task:
          shell: echo "deep"
    "#;

  // Create directory structure and write files
  let root_path = temp_dir.path().join("Octafile.yml");
  let nested_dir = temp_dir.path().join("nested");
  let deep_dir = nested_dir.join("deep");
  std::fs::create_dir(&nested_dir)?;
  std::fs::create_dir(&deep_dir)?;
  let nested_path = nested_dir.join("Octafile.yml");
  let deep_path = deep_dir.join("Octafile.yml");

  std::fs::write(&root_path, root_content)?;
  std::fs::write(&nested_path, nested_content)?;
  std::fs::write(&deep_path, deep_content)?;

  // Load the root octafile
  Ok(Octafile::load(
    Some(root_path),
    false,
    vec!["shell".to_string()],
    "shell",
  )?)
}
