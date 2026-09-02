use super::*;
use std::{fs, time::Duration};
use tempfile::TempDir;

struct ConstantSourceStrategy;

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

  let first = task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new())
    .await
    .unwrap()
    .unwrap();
  assert!(first.should_run());
  first.commit(&database).unwrap();
  assert!(!task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new())
    .await
    .unwrap()
    .unwrap()
    .should_run());

  fs::write(source, "changed").unwrap();
  assert!(task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new())
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

  let first = task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new())
    .await
    .unwrap()
    .unwrap();
  first.commit(&database).unwrap();
  fs::write(source, "changed").unwrap();

  assert!(!task
    .standalone_freshness(&database, false, &vars, &envs, &CancellationToken::new())
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
      plugin_manager.clone(),
      cache,
      fingerprint,
      false,
      false,
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result.trim(), "hello world");
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
      plugin_manager.clone(),
      cache,
      fingerprint,
      false,
      false,
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result, "Hello world!");
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
      plugin_manager.clone(),
      cache.clone(),
      fingerprint.clone(),
      false,
      false,
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result1.trim(), "cached result");

  // Second execution should return cached result
  let result2 = task
    .execute(
      plugin_manager.clone(),
      cache.clone(),
      fingerprint.clone(),
      false,
      false,
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result1, result2);
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
      plugin_manager.clone(),
      cache,
      fingerprint,
      false,
      false,
      CancellationToken::new(),
    )
    .await;
  assert!(matches!(result, Err(ExecutorError::TaskFailed(_))));
  plugin_manager.shutdown_all().await;
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
    .execute(plugin_manager.clone(), cache, fingerprint, false, false, cancel_token)
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
      plugin_manager.clone(),
      cache.clone(),
      db.clone(),
      false,
      false,
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
    .execute(
      plugin_manager.clone(),
      cache,
      db,
      false,
      false,
      CancellationToken::new(),
    )
    .await;

  assert_eq!(result.unwrap(), "reusable");
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
      plugin_manager.clone(),
      cache,
      fingerprint,
      false,
      false,
      CancellationToken::new(),
    )
    .await;
  assert!(result.is_ok());
  assert_eq!(result.unwrap(), "");
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
      plugin_manager.clone(),
      cache,
      fingerprint,
      false,
      false,
      CancellationToken::new(),
    )
    .await
    .unwrap();
  assert_eq!(result, "Result: dep_output");
  plugin_manager.shutdown_all().await;
}
