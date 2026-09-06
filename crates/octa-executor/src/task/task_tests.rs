use super::*;
use std::{
  fs,
  sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex as StdMutex,
  },
  time::Duration,
};
use tempfile::TempDir;

struct ConstantSourceStrategy;

struct CountingResolver(AtomicUsize);

#[async_trait]
impl VariableResolver for CountingResolver {
  async fn resolve(&self, _prompt: &crate::vars::VariablePrompt) -> Result<String, String> {
    self.0.fetch_add(1, Ordering::SeqCst);
    Ok("resolved".to_owned())
  }
}

#[async_trait]
impl SourceStrategy for ConstantSourceStrategy {
  fn key(&self) -> &'static str {
    "constant"
  }

  async fn fingerprint(&self, _sources: &[PathBuf], _cancel_token: &CancellationToken) -> ExecutorResult<Vec<u8>> {
    Ok(vec![1])
  }
}

fn plugin(key: &str, value: impl Into<Value>) -> Option<PluginInvocation> {
  Some(PluginInvocation::new(key.to_owned(), value.into()))
}

struct UnscopedTaskItem;

struct LegacyScopedTaskItem(ConsoleScope);

impl TaskItem for UnscopedTaskItem {
  fn run_mode(&self) -> RunMode {
    RunMode::Always
  }

  fn failfast(&self) -> bool {
    false
  }

  fn requires_concurrency_permit(&self) -> bool {
    true
  }
}

impl TaskItem for LegacyScopedTaskItem {
  fn run_mode(&self) -> RunMode {
    RunMode::Always
  }

  fn failfast(&self) -> bool {
    false
  }

  fn requires_concurrency_permit(&self) -> bool {
    true
  }

  fn output_scope(&self) -> Option<ConsoleScope> {
    Some(self.0.clone())
  }
}

#[test]
fn task_items_are_unscoped_by_default() {
  let task = UnscopedTaskItem;

  assert_eq!(task.run_mode(), RunMode::Always);
  assert!(!task.failfast());
  assert!(task.requires_concurrency_permit());
  assert!(task.output_scope().is_none());
  assert!(task.execution_binding().is_none());
}

#[test]
fn task_level_output_scope_is_upgraded_to_an_execution_binding() {
  let scope = octa_output::ConsoleScopeAllocator::default().scope("task");
  let task = LegacyScopedTaskItem(scope.clone());

  assert_eq!(task.run_mode(), RunMode::Always);
  assert!(!task.failfast());
  assert!(task.requires_concurrency_permit());
  let binding = task.execution_binding().unwrap();
  assert_eq!(binding.scope(), &scope);
  assert!(binding.step().is_none());
}

#[tokio::test]
async fn invocation_nodes_resolve_required_variables_once() {
  let temp_dir = TempDir::new().unwrap();
  let configured = serde_yml::from_str("VALUE:\n  required: prompt\n").unwrap();
  let resolver = Arc::new(CountingResolver(AtomicUsize::new(0)));
  let invocation = Arc::new(InvocationRuntime::new(
    Vars::with_variables(configured),
    EnvironmentPlan::default(),
    HashSet::new(),
    Some(resolver.clone()),
  ));
  assert!(format!("{invocation:?}").contains("initialized: false"));
  let make_task = |id: &str| {
    TaskNode::new(
      TaskConfig::builder()
        .id(id)
        .name(id)
        .dep_name(id)
        .dir(temp_dir.path())
        .invocation_runtime(Some(invocation.clone()))
        .build()
        .unwrap(),
    )
  };
  let first = make_task("first");
  let second = make_task("second");
  let evaluator: Arc<dyn PluginEvaluator> = Arc::new(ManagerPluginEvaluator::new(Arc::new(PluginManager::new(
    temp_dir.path(),
  ))));

  let (first_context, second_context) = tokio::join!(
    first.resolve_runtime_context(evaluator.clone(), false, CancellationToken::new(), None),
    second.resolve_runtime_context(evaluator, false, CancellationToken::new(), None),
  );

  assert_eq!(
    first_context.unwrap().vars.get("VALUE"),
    Some(&Value::String("resolved".to_owned()))
  );
  assert_eq!(
    second_context.unwrap().vars.get("VALUE"),
    Some(&Value::String("resolved".to_owned()))
  );
  assert_eq!(resolver.0.load(Ordering::SeqCst), 1);
  assert!(format!("{invocation:?}").contains("initialized: true"));
}

// Helper function to create a test TaskNode
fn create_test_task(name: &str, cmd: Option<&str>, tpl: Option<String>, run_mode: Option<RunMode>) -> TaskNode {
  let plugin = tpl
    .map(|value| PluginInvocation::new("tpl".to_owned(), Value::String(value)))
    .or_else(|| cmd.map(|value| PluginInvocation::new("shell".to_owned(), Value::String(value.to_owned()))));

  let task_config = TaskConfig::builder()
    .id(name.to_string())
    .name(name.to_string())
    .dep_name(name.to_string())
    .dir(PathBuf::from("."))
    .vars(Vars::new())
    .envs(Envs::new())
    .plugin(plugin)
    .run_mode(Some(run_mode.unwrap_or(RunMode::Always)))
    .build()
    .unwrap();

  TaskNode::new(task_config)
}

fn runtime(
  plugin_manager: Arc<PluginManager>,
  cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
  fingerprint: Arc<Db>,
) -> TaskRuntime {
  TaskRuntime {
    plugin_manager,
    cache,
    fingerprint,
    console: Arc::new(Console::default()),
    run_id: 1,
    dry: false,
    force: false,
    deferred_exit_code: None,
  }
}

#[derive(Clone)]
struct RecordingRenderer(Arc<StdMutex<Vec<octa_output::ConsoleRecord>>>);

impl octa_output::ConsoleRenderer for RecordingRenderer {
  fn render(&mut self, entry: &octa_output::ConsoleEntry) -> io::Result<()> {
    self.0.lock().unwrap().push(entry.record().clone());
    Ok(())
  }
}

async fn prepare_dir(task: &TaskNode, dry: bool) -> ExecutorResult<PathBuf> {
  let mut vars = task.vars.clone();
  vars.expand(dry).await?;
  task.prepare_dir_with_vars(&vars, dry).await
}

#[tokio::test]
async fn test_prepare_dir_creates_interpolated_directory() {
  let temp_dir = TempDir::new().unwrap();
  let target = temp_dir.path().join("build").join("generated");
  let mut task = create_test_task("create_dir", None, None, None);
  let mut vars = Vars::new();
  vars.insert("OUTPUT_DIR", &target.to_string_lossy().to_string());
  task.vars = vars;
  task.dir = PathBuf::from("{{ OUTPUT_DIR }}");

  let prepared = prepare_dir(&task, false).await.unwrap();

  assert!(target.is_dir());
  assert_eq!(prepared, canonicalize(target).unwrap());
}

#[tokio::test]
async fn test_prepare_dir_dry_run_with_existing_directory() {
  let temp_dir = TempDir::new().unwrap();
  let mut task = create_test_task("existing_dir", None, None, None);
  task.dir = temp_dir.path().to_path_buf();

  let prepared = prepare_dir(&task, true).await.unwrap();

  assert_eq!(prepared, canonicalize(temp_dir.path()).unwrap());
}

#[tokio::test]
async fn test_prepare_dir_dry_run_with_missing_absolute_directory() {
  let temp_dir = TempDir::new().unwrap();
  let target = temp_dir.path().join("missing");
  let mut task = create_test_task("missing_absolute_dir", None, None, None);
  task.dir = target.clone();

  let prepared = prepare_dir(&task, true).await.unwrap();

  assert_eq!(prepared, target);
  assert!(!prepared.exists());
}

#[tokio::test]
async fn test_prepare_dir_dry_run_with_missing_relative_directory() {
  let current_dir = env::current_dir().unwrap();
  let temp_dir = TempDir::new_in(&current_dir).unwrap();
  let relative = temp_dir.path().strip_prefix(&current_dir).unwrap().join("missing");
  let mut task = create_test_task("missing_relative_dir", None, None, None);
  task.dir = relative.clone();

  let prepared = prepare_dir(&task, true).await.unwrap();

  assert_eq!(prepared, current_dir.join(relative));
  assert!(!prepared.exists());
}

#[tokio::test]
async fn test_prepare_dir_rejects_file_path() {
  let temp_file = tempfile::NamedTempFile::new().unwrap();
  let mut task = create_test_task("file_path", None, None, None);
  task.dir = temp_file.path().to_path_buf();

  assert!(matches!(
    prepare_dir(&task, false).await,
    Err(ExecutorError::IoError(_))
  ));
}

#[tokio::test]
async fn standalone_task_config_preserves_source_freshness() {
  let root = TempDir::new().unwrap();
  let source = root.path().join("source.txt");
  fs::write(&source, "initial").unwrap();
  let task = TaskNode::new(
    TaskConfig::builder()
      .id("standalone")
      .name("standalone")
      .dep_name("standalone")
      .dir(root.path())
      .sources(Some(vec![source.to_string_lossy().into_owned()]))
      .octafile_root(root.path())
      .source_strategy(Some(SourceMethod::Hash))
      .build()
      .unwrap(),
  );
  let database = sled::Config::new().temporary(true).open().unwrap();
  let vars = Vars::new();
  let envs = Envs::new();
  let output = ConsoleTarget::new(Arc::new(Console::default()), 7, None);

  let first = task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new(), &output)
    .await
    .unwrap()
    .unwrap();
  assert!(first.should_run());
  first.commit(&database).unwrap();
  assert!(!task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new(), &output)
    .await
    .unwrap()
    .unwrap()
    .should_run());

  fs::write(source, "changed").unwrap();
  assert!(task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new(), &output)
    .await
    .unwrap()
    .unwrap()
    .should_run());
}

#[tokio::test]
async fn standalone_task_uses_an_injected_source_strategy() {
  let root = TempDir::new().unwrap();
  let source = root.path().join("source.txt");
  fs::write(&source, "initial").unwrap();
  let task = TaskNode::new(
    TaskConfig::builder()
      .id("custom-strategy")
      .name("custom-strategy")
      .dep_name("custom-strategy")
      .dir(root.path())
      .sources(Some(vec![source.to_string_lossy().into_owned()]))
      .octafile_root(root.path())
      .source_strategy_provider(SourceMethod::custom("constant"), ConstantSourceStrategy)
      .build()
      .unwrap(),
  );
  let freshness = task.standalone_freshness.as_ref().unwrap();
  assert_eq!(freshness.method(), SourceMethod::custom("constant"));
  assert_eq!(freshness.strategy_key(), "constant");
  let database = sled::Config::new().temporary(true).open().unwrap();
  let vars = Vars::new();
  let envs = Envs::new();
  let output = ConsoleTarget::new(Arc::new(Console::default()), 7, None);

  let first = task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new(), &output)
    .await
    .unwrap()
    .unwrap();
  first.commit(&database).unwrap();
  fs::write(source, "changed").unwrap();

  assert!(!task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new(), &output)
    .await
    .unwrap()
    .unwrap()
    .should_run());
}

#[test]
fn standalone_task_rejects_an_unregistered_source_strategy() {
  let result = TaskConfig::builder()
    .id("custom-strategy")
    .name("custom-strategy")
    .dep_name("custom-strategy")
    .dir(".")
    .sources(Some(vec!["source.txt".to_owned()]))
    .source_strategy(Some(SourceMethod::custom("missing")))
    .build();

  assert!(matches!(
    result,
    Err(ExecutorError::SourceStrategyUnavailable(name)) if name == "missing"
  ));
}

#[test]
fn task_config_builder_returns_typed_missing_field_errors() {
  let result = TaskConfig::builder().name("task").dep_name("task").dir(".").build();

  assert!(matches!(result, Err(ExecutorError::TaskConfigFieldMissing("id"))));
}

#[tokio::test]
async fn test_prepare_dir_propagates_interpolation_error() {
  let mut task = create_test_task("invalid_interpolation", None, None, None);
  task.dir = PathBuf::from("{{ invalid + }}");

  assert!(matches!(
    prepare_dir(&task, false).await,
    Err(ExecutorError::ValueExpandError(_, _))
  ));
}

#[cfg(unix)]
#[tokio::test]
async fn test_prepare_dir_dry_run_propagates_canonicalize_error() {
  use std::os::unix::fs::PermissionsExt;

  let temp_dir = TempDir::new().unwrap();
  let locked = temp_dir.path().join("locked");
  fs::create_dir(&locked).unwrap();
  fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

  let mut task = create_test_task("unreadable_dir", None, None, None);
  task.dir = locked.join("child");
  let result = prepare_dir(&task, true).await;

  fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
  assert!(matches!(result, Err(ExecutorError::IoError(_))));
}

#[tokio::test]
async fn test_basic_command_execution() {
  let db = sled::Config::new()
    .temporary(true)
    .open()
    .expect("Failed to open in-memory Sled database");

  let task = create_test_task("test_task", Some("echo hello world"), None, None);

  let cache = Arc::new(Mutex::new(IndexMap::new()));
  let fingerprint = Arc::new(db);
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_tpl";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_tpl.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  let result = task
    .execute(
      runtime(plugin_manager.clone(), cache, fingerprint),
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result.output().trim(), "hello world");
  plugin_manager.shutdown_all().await;
}

#[tokio::test]
async fn plugin_stdout_and_stderr_are_routed_as_structured_events() {
  let scope = octa_output::ConsoleScopeAllocator::default().scope("output");
  let task = TaskNode::new(
    TaskConfig::builder()
      .id("output")
      .name("output")
      .dep_name("output")
      .dir(".")
      .execution_binding(Some(ExecutionBinding::for_task(scope.clone())))
      .plugin(plugin("shell", "echo stdout && echo stderr 1>&2"))
      .build()
      .unwrap(),
  );
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{project_root}/../../plugins")));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();
  let events = Arc::new(StdMutex::new(Vec::new()));
  let console = Arc::new(Console::new(RecordingRenderer(events.clone())));
  let runtime = TaskRuntime {
    plugin_manager: plugin_manager.clone(),
    cache: Arc::new(Mutex::new(IndexMap::new())),
    fingerprint: Arc::new(sled::Config::new().temporary(true).open().unwrap()),
    console,
    run_id: 7,
    dry: false,
    force: false,
    deferred_exit_code: None,
  };

  task.execute(runtime, CancellationToken::new()).await.unwrap();

  {
    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
      event,
      octa_output::ConsoleRecord::Execution(octa_output::ExecutionEvent::Output {
        run_id: 7,
        scope: event_scope,
        stream: octa_output::ConsoleStream::Stdout,
        payload: octa_output::ConsolePayload::Line(line),
        ..
      }) if event_scope.as_ref() == Some(&scope) && line == "stdout"
    )));
    assert!(events.iter().any(|event| matches!(
      event,
      octa_output::ConsoleRecord::Execution(octa_output::ExecutionEvent::Output {
        run_id: 7,
        scope: event_scope,
        stream: octa_output::ConsoleStream::Stderr,
        payload: octa_output::ConsolePayload::Line(line),
        ..
      }) if event_scope.as_ref() == Some(&scope) && line == "stderr"
    )));
  }
  plugin_manager.shutdown_all().await;
}

#[tokio::test]
async fn test_template_rendering() {
  let db = sled::Config::new()
    .temporary(true)
    .open()
    .expect("Failed to open in-memory Sled database");

  let mut vars = Vars::new();
  vars.insert("name", &"world");

  let task_config = TaskConfig::builder()
    .id("template_task".to_string())
    .name("template_task".to_string())
    .dep_name("template_task".to_string())
    .dir(PathBuf::from("."))
    .vars(vars)
    .envs(Envs::new())
    .plugin(plugin("tpl", "Hello {{ name }}!"))
    .build()
    .unwrap();

  let task = TaskNode::new(task_config);

  let cache = Arc::new(Mutex::new(IndexMap::new()));
  let fingerprint = Arc::new(db);
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_tpl";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_tpl.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  let result = task
    .execute(
      runtime(plugin_manager.clone(), cache, fingerprint),
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result.output(), "Hello world!");
  plugin_manager.shutdown_all().await;
}

#[tokio::test]
async fn test_cache_behavior() {
  let db = sled::Config::new()
    .temporary(true)
    .open()
    .expect("Failed to open in-memory Sled database");
  let task = create_test_task("cache_task", Some("echo cached result"), None, Some(RunMode::Once));

  let cache = Arc::new(Mutex::new(IndexMap::new()));
  let fingerprint = Arc::new(db);
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_tpl";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_tpl.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  // First execution
  let result1 = task
    .execute(
      runtime(plugin_manager.clone(), cache.clone(), fingerprint.clone()),
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result1.output().trim(), "cached result");

  // Second execution should return cached result
  let result2 = task
    .execute(
      runtime(plugin_manager.clone(), cache.clone(), fingerprint.clone()),
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result1.output(), result2.output());
  assert_eq!(result1.status(), octa_output::ConsoleStatus::Success);
  assert_eq!(result2.status(), octa_output::ConsoleStatus::Skipped);
  plugin_manager.shutdown_all().await;
}

#[tokio::test]
async fn test_error_handling() {
  let db = sled::Config::new()
    .temporary(true)
    .open()
    .expect("Failed to open in-memory Sled database");

  let task = create_test_task("error_task", Some("nonexistent_command"), None, None);

  let cache = Arc::new(Mutex::new(IndexMap::new()));
  let fingerprint = Arc::new(db);
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_tpl";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_tpl.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  let result = task
    .execute(
      runtime(plugin_manager.clone(), cache, fingerprint),
      CancellationToken::new(),
    )
    .await;
  assert!(matches!(
    result,
    Err(ExecutorError::CommandFailed { task, code, .. }) if task == "error_task" && code != 0
  ));
  plugin_manager.shutdown_all().await;
}

#[tokio::test]
async fn failed_precondition_cancels_the_task_before_plugin_execution() {
  let task = TaskNode::new(
    TaskConfig::builder()
      .id("guarded")
      .name("guarded")
      .dep_name("guarded")
      .dir(".")
      .preconditions(Some(vec!["false".to_owned()]))
      .plugin(plugin("missing", "must not run"))
      .build()
      .unwrap(),
  );
  let plugin_manager = Arc::new(PluginManager::new(TempDir::new().unwrap().path()));
  let result = task
    .execute(
      runtime(
        plugin_manager,
        Arc::new(Mutex::new(IndexMap::new())),
        Arc::new(sled::Config::new().temporary(true).open().unwrap()),
      ),
      CancellationToken::new(),
    )
    .await;

  assert!(matches!(result, Err(ExecutorError::TaskCancelled(message)) if message.contains("preconditions failed")));
}

#[tokio::test]
async fn missing_plugin_errors_can_be_propagated_or_ignored() {
  for ignore_errors in [false, true] {
    let task = TaskNode::new(
      TaskConfig::builder()
        .id("missing-plugin")
        .name("missing-plugin")
        .dep_name("missing-plugin")
        .dir(".")
        .plugin(plugin("missing", "value"))
        .ignore_errors(Some(ignore_errors))
        .build()
        .unwrap(),
    );
    let events = Arc::new(StdMutex::new(Vec::new()));
    let result = task
      .execute(
        TaskRuntime {
          plugin_manager: Arc::new(PluginManager::new(TempDir::new().unwrap().path())),
          cache: Arc::new(Mutex::new(IndexMap::new())),
          fingerprint: Arc::new(sled::Config::new().temporary(true).open().unwrap()),
          console: Arc::new(Console::new(RecordingRenderer(events.clone()))),
          run_id: 7,
          dry: false,
          force: false,
          deferred_exit_code: None,
        },
        CancellationToken::new(),
      )
      .await;

    if ignore_errors {
      assert_eq!(result.unwrap().output(), "");
      assert!(events.lock().unwrap().iter().any(|event| matches!(
        event,
        octa_output::ConsoleRecord::Diagnostic(octa_output::ConsoleDiagnostic {
          run_id: Some(7),
          level: octa_output::ConsoleLevel::Error,
          message,
          ..
        }) if message.contains("failed but errors ignored")
      )));
    } else {
      assert!(result.is_err());
    }
  }
}

#[tokio::test]
async fn test_task_cancellation() {
  let db = sled::Config::new()
    .temporary(true)
    .open()
    .expect("Failed to open in-memory Sled database");

  let cancel_token = CancellationToken::new();
  let task_config = TaskConfig::builder()
    .id("long_task".to_string())
    .name("long_task".to_string())
    .dep_name("long_task".to_string())
    .dir(PathBuf::from("."))
    .vars(Vars::new())
    .envs(Envs::new())
    .plugin(plugin("shell", "sleep 5"))
    .build()
    .unwrap();

  let task = TaskNode::new(task_config);

  let cache = Arc::new(Mutex::new(IndexMap::new()));
  let fingerprint = Arc::new(db);
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_tpl";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_tpl.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  // Cancel the task after a short delay
  let cancel_handle = tokio::spawn({
    let cancel_token = cancel_token.clone();
    async move {
      tokio::time::sleep(Duration::from_millis(100)).await;
      cancel_token.cancel();
    }
  });

  let result = task
    .execute(runtime(plugin_manager.clone(), cache, fingerprint), cancel_token)
    .await;
  assert!(matches!(result, Err(ExecutorError::TaskCancelled(_))));

  cancel_handle.await.unwrap();
  plugin_manager.shutdown_all().await;
}

#[tokio::test]
async fn test_task_timeout_stops_command_and_keeps_plugin_reusable() {
  let db = Arc::new(sled::Config::new().temporary(true).open().unwrap());
  let cache = Arc::new(Mutex::new(IndexMap::new()));
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../target/debug", project_root)));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  let long_command = "while true; do :; done";
  let timeout = serde_yml::from_str::<Timeout>("100ms").unwrap();
  let timed_task = TaskNode::new(
    TaskConfig::builder()
      .id("timed_task")
      .name("timed_task")
      .dep_name("timed_task")
      .dir(".")
      .plugin(plugin("shell", long_command))
      .timeout(Some(timeout))
      .build()
      .unwrap(),
  );

  let result = timed_task
    .execute(
      runtime(plugin_manager.clone(), cache.clone(), db.clone()),
      CancellationToken::new(),
    )
    .await;
  assert!(matches!(result, Err(ExecutorError::TaskTimedOut { .. })));

  let next_task = TaskNode::new(
    TaskConfig::builder()
      .id("next_task")
      .name("next_task")
      .dep_name("next_task")
      .dir(".")
      .plugin(plugin("shell", "echo reusable"))
      .build()
      .unwrap(),
  );
  let result = next_task
    .execute(runtime(plugin_manager.clone(), cache, db), CancellationToken::new())
    .await;

  assert_eq!(result.unwrap().output(), "reusable");
  plugin_manager.shutdown_all().await;
}

#[tokio::test]
async fn test_ignore_errors() {
  let db = sled::Config::new()
    .temporary(true)
    .open()
    .expect("Failed to open in-memory Sled database");

  let task_config = TaskConfig::builder()
    .id("ignore_error_task".to_string())
    .name("ignore_error_task".to_string())
    .dep_name("ignore_error_task".to_string())
    .dir(PathBuf::from("."))
    .vars(Vars::new())
    .envs(Envs::new())
    .plugin(plugin("shell", "nonexistent_command"))
    .ignore_errors(Some(true))
    .build()
    .unwrap();

  let task = TaskNode::new(task_config);

  let cache = Arc::new(Mutex::new(IndexMap::new()));
  let fingerprint = Arc::new(db);
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_tpl";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_tpl.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  let result = task
    .execute(
      runtime(plugin_manager.clone(), cache, fingerprint),
      CancellationToken::new(),
    )
    .await;
  assert!(result.is_ok());
  assert_eq!(result.unwrap().output(), "");
  plugin_manager.shutdown_all().await;
}

#[tokio::test]
async fn test_dependency_results() {
  let db = sled::Config::new()
    .temporary(true)
    .open()
    .expect("Failed to open in-memory Sled database");

  let task = create_test_task(
    "dep_task",
    None,
    Some("Result: {{ deps_result.dep1 }}".to_owned()),
    None,
  );

  task.set_result("dep1".to_string(), "dep_output".to_string()).await;

  let cache = Arc::new(Mutex::new(IndexMap::new()));
  let fingerprint = Arc::new(db);
  let project_root = env!("CARGO_MANIFEST_DIR");
  let plugin_manager = Arc::new(PluginManager::new(format!("{}/../../plugins", project_root)));
  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_shell";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_shell.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  #[cfg(not(windows))]
  let plugin_name = "octa_plugin_tpl";
  #[cfg(windows)]
  let plugin_name = "octa_plugin_tpl.exe";
  plugin_manager.start_plugin(plugin_name).await.unwrap();

  let result = task
    .execute(
      runtime(plugin_manager.clone(), cache, fingerprint),
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result.output(), "Result: dep_output");
  plugin_manager.shutdown_all().await;
}
