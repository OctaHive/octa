use octa_plugin::protocol::Schema;
use octa_plugin::socket::{interpret_local_socket_name, make_local_socket_name};
use std::ffi::OsString;
use std::{
  collections::{HashMap, HashSet, VecDeque},
  path::{Path, PathBuf},
  sync::{Arc, Mutex as StdMutex},
};
use thiserror::Error;
use tokio::io::{self, AsyncReadExt};
use tokio::{
  sync::{Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock},
  task::JoinHandle,
  time::{timeout, Duration},
};
use uuid::Uuid;

use crate::{
  plugin_client::PluginClient,
  plugin_process::{LocalPluginLauncher, PluginLaunchError, PluginLaunchRequest, PluginLauncher, PluginProcess},
};

const PLUGIN_START_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(1);
const STARTUP_DIAGNOSTIC_LIMIT: usize = 64 * 1024;

#[derive(Error, Debug)]
pub enum PluginManagerError {
  #[error("Plugin not found: {0}")]
  PluginNotFound(String),

  #[error("Plugin already running: {0}")]
  PluginAlreadyRunning(String),

  #[error("Plugin selector already registered: {0}")]
  PluginSelectorAlreadyRegistered(String),

  #[error("Failed to start plugin: {0}")]
  StartError(String),

  #[error("Failed to shutdown plugin: {0}")]
  ShutdownError(String),

  #[error("Plugin connection error: {0}")]
  ConnectionError(String),

  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),

  #[error(transparent)]
  Launch(#[from] PluginLaunchError),

  #[error("Socket path error: {0}")]
  SocketPath(String),

  #[error("Pipe error: {0}")]
  PipeError(String),
}

type Result<T> = std::result::Result<T, PluginManagerError>;

struct PluginInstance {
  process: PluginProcess,
  socket_path: OsString,
  client: PluginClient,
}

struct StartupReservation {
  names: Arc<StdMutex<HashSet<String>>>,
  name: String,
}

impl Drop for StartupReservation {
  fn drop(&mut self) {
    if let Ok(mut names) = self.names.lock() {
      names.remove(&self.name);
    }
  }
}

#[derive(Clone)]
pub struct PluginRegistration {
  plugin_name: String,
  schema: Schema,
  validator: Option<Arc<jsonschema::Validator>>,
}

impl PluginRegistration {
  fn new(plugin_name: String, schema: Schema) -> std::result::Result<Self, String> {
    let validator = schema
      .validation_schema
      .clone()
      .map(|schema| jsonschema::validator_for(&serde_json::Value::Object(schema)).map(Arc::new))
      .transpose()
      .map_err(|error| error.to_string())?;
    Ok(Self {
      plugin_name,
      schema,
      validator,
    })
  }

  pub fn plugin_name(&self) -> &str {
    &self.plugin_name
  }

  pub fn supports_raw(&self) -> bool {
    self.schema.supports_raw
  }

  pub fn validate(&self, value: &serde_json::Value) -> std::result::Result<(), String> {
    let Some(validator) = &self.validator else {
      return Ok(());
    };
    let errors = validator
      .iter_errors(value)
      .map(|error| error.to_string())
      .collect::<Vec<_>>();
    if errors.is_empty() {
      Ok(())
    } else {
      Err(errors.join("; "))
    }
  }
}

#[derive(Default)]
struct PluginRegistry {
  keys: HashMap<String, PluginRegistration>,
  capabilities: HashMap<String, PluginRegistration>,
}

impl PluginRegistry {
  fn register(&mut self, registration: PluginRegistration) -> Result<()> {
    let schema = &registration.schema;
    if schema.key.is_empty() || self.keys.contains_key(&schema.key) {
      return Err(PluginManagerError::PluginSelectorAlreadyRegistered(schema.key.clone()));
    }
    let mut capabilities = HashSet::new();
    for capability in &schema.capabilities {
      if capability.is_empty() || !capabilities.insert(capability) || self.capabilities.contains_key(capability) {
        return Err(PluginManagerError::PluginSelectorAlreadyRegistered(capability.clone()));
      }
    }

    self.keys.insert(schema.key.clone(), registration.clone());
    for capability in &schema.capabilities {
      self.capabilities.insert(capability.clone(), registration.clone());
    }
    Ok(())
  }

  fn remove_plugin(&mut self, plugin_name: &str) {
    self
      .keys
      .retain(|_, registration| registration.plugin_name != plugin_name);
    self
      .capabilities
      .retain(|_, registration| registration.plugin_name != plugin_name);
  }
}

pub struct PluginManager {
  plugins_dir: PathBuf,
  workspace: PathBuf,
  launcher: Arc<dyn PluginLauncher>,
  active_plugins: Arc<Mutex<HashMap<String, PluginInstance>>>,
  plugin_registry: Arc<Mutex<PluginRegistry>>,
  starting_plugins: Arc<StdMutex<HashSet<String>>>,
  execution_lock: Arc<RwLock<()>>,
}

/// Prevents an interactive command from overlapping another plugin invocation.
pub enum PluginExecutionGuard {
  Shared(OwnedRwLockReadGuard<()>),
  Exclusive(OwnedRwLockWriteGuard<()>),
}

impl PluginManager {
  /// Creates a local plugin manager and captures the current directory as its workspace.
  /// Embedded hosts serving multiple workspaces should use [`Self::with_workspace`].
  pub fn new(plugins_dir: impl Into<PathBuf>) -> Self {
    Self::with_workspace(
      plugins_dir,
      std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    )
  }

  /// Creates a local plugin manager rooted in an explicit workspace.
  pub fn with_workspace(plugins_dir: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
    Self::with_launcher(plugins_dir, workspace, Arc::new(LocalPluginLauncher))
  }

  /// Creates a manager with an explicit workspace and application-owned process launcher.
  pub fn with_launcher(
    plugins_dir: impl Into<PathBuf>,
    workspace: impl Into<PathBuf>,
    launcher: Arc<dyn PluginLauncher>,
  ) -> Self {
    let workspace = workspace.into();
    let plugins_dir = plugins_dir.into();
    let plugins_dir = if plugins_dir.is_absolute() {
      plugins_dir
    } else {
      workspace.join(plugins_dir)
    };
    Self {
      plugins_dir,
      workspace,
      launcher,
      active_plugins: Arc::new(Mutex::new(HashMap::new())),
      plugin_registry: Arc::new(Mutex::new(PluginRegistry::default())),
      starting_plugins: Arc::new(StdMutex::new(HashSet::new())),
      execution_lock: Arc::new(RwLock::new(())),
    }
  }

  pub async fn execution_guard(&self, raw: bool) -> PluginExecutionGuard {
    if raw {
      PluginExecutionGuard::Exclusive(self.execution_lock.clone().write_owned().await)
    } else {
      PluginExecutionGuard::Shared(self.execution_lock.clone().read_owned().await)
    }
  }

  /// Generate a unique socket path for a plugin
  fn generate_socket_path(&self) -> OsString {
    make_local_socket_name(&Uuid::new_v4().simple().to_string())
  }

  fn reserve_start(&self, plugin_name: &str) -> Result<StartupReservation> {
    let mut names = self
      .starting_plugins
      .lock()
      .map_err(|error| PluginManagerError::StartError(error.to_string()))?;
    if !names.insert(plugin_name.to_owned()) {
      return Err(PluginManagerError::PluginAlreadyRunning(plugin_name.to_owned()));
    }
    Ok(StartupReservation {
      names: self.starting_plugins.clone(),
      name: plugin_name.to_owned(),
    })
  }

  async fn read_stream_tail<R>(mut stream: R) -> io::Result<String>
  where
    R: tokio::io::AsyncRead + Unpin,
  {
    let mut buffer = [0; 8 * 1024];
    let mut tail = VecDeque::with_capacity(STARTUP_DIAGNOSTIC_LIMIT);
    let mut truncated = false;

    loop {
      let count = stream.read(&mut buffer).await?;
      if count == 0 {
        break;
      }
      let overflow = tail
        .len()
        .saturating_add(count)
        .saturating_sub(STARTUP_DIAGNOSTIC_LIMIT);
      if overflow > 0 {
        tail.drain(..overflow);
        truncated = true;
      }
      tail.extend(&buffer[..count]);
    }

    let bytes = tail.into_iter().collect::<Vec<_>>();
    let output = String::from_utf8_lossy(&bytes);
    if truncated {
      Ok(format!("[earlier plugin output truncated]\n{output}"))
    } else {
      Ok(output.into_owned())
    }
  }

  fn resolve_plugin_command(plugin_path: &Path) -> (PathBuf, Vec<OsString>) {
    if plugin_path.extension().and_then(|extension| extension.to_str()) == Some("py") {
      (PathBuf::from("python3"), vec![plugin_path.as_os_str().to_owned()])
    } else {
      (plugin_path.to_owned(), Vec::new())
    }
  }

  async fn collect_startup_output(handle: JoinHandle<io::Result<String>>) -> String {
    match timeout(PROCESS_STOP_TIMEOUT, handle).await {
      Ok(Ok(Ok(output))) => output,
      Ok(Ok(Err(error))) => format!("failed to read plugin output: {error}"),
      Ok(Err(error)) => format!("failed to join plugin output task: {error}"),
      Err(_) => "timed out while reading plugin output".to_string(),
    }
  }

  async fn cleanup_failed_start(
    process: &mut PluginProcess,
    socket_path: &OsString,
    stdout_handle: JoinHandle<io::Result<String>>,
    stderr_handle: JoinHandle<io::Result<String>>,
  ) -> String {
    let _ = process.start_kill();
    let _ = timeout(PROCESS_STOP_TIMEOUT, process.wait()).await;
    let _ = tokio::fs::remove_file(socket_path).await;

    let (stdout, stderr) = tokio::join!(
      Self::collect_startup_output(stdout_handle),
      Self::collect_startup_output(stderr_handle)
    );

    let mut diagnostics = String::new();
    if !stdout.is_empty() {
      diagnostics.push_str(&format!("\nPlugin stdout: {stdout}"));
    }
    if !stderr.is_empty() {
      diagnostics.push_str(&format!("\nPlugin stderr: {stderr}"));
    }
    diagnostics
  }

  fn with_startup_diagnostics(error: PluginManagerError, diagnostics: String) -> PluginManagerError {
    if diagnostics.is_empty() {
      return error;
    }

    match error {
      PluginManagerError::StartError(message) => PluginManagerError::StartError(format!("{message}{diagnostics}")),
      PluginManagerError::ConnectionError(message) => {
        PluginManagerError::ConnectionError(format!("{message}{diagnostics}"))
      },
      error => error,
    }
  }

  /// Start a plugin and establish connection
  pub async fn start_plugin(&self, plugin_path: &str) -> Result<Schema> {
    let plugin_full_path = self.plugins_dir.join(plugin_path);
    let plugin_name = plugin_full_path
      .file_stem()
      .map(|stem| stem.to_string_lossy().into_owned())
      .map(|stem| stem.strip_prefix("octa_plugin_").map(|s| s.to_owned()).unwrap_or(stem))
      .unwrap_or_else(|| "(unknown)".into());

    let _reservation = self.reserve_start(&plugin_name)?;

    if self.active_plugins.lock().await.contains_key(&plugin_name) {
      return Err(PluginManagerError::PluginAlreadyRunning(plugin_name.to_string()));
    }

    let socket_path = self.generate_socket_path();

    if !plugin_full_path.exists() {
      return Err(PluginManagerError::PluginNotFound(
        plugin_full_path.to_string_lossy().to_string(),
      ));
    }

    let socket_name =
      interpret_local_socket_name(&socket_path).map_err(|e| PluginManagerError::SocketPath(e.to_string()))?;

    let (executable, arguments) = Self::resolve_plugin_command(&plugin_full_path);
    let mut process = self
      .launcher
      .launch(PluginLaunchRequest {
        executable,
        arguments,
        environment: HashMap::new(),
        workspace: self.workspace.clone(),
        socket_path: PathBuf::from(&socket_path),
      })
      .await?;

    let stdout = process
      .take_stdout()
      .ok_or_else(|| PluginManagerError::PipeError("Failed to capture stdout".to_string()))?;
    let stderr = process
      .take_stderr()
      .ok_or_else(|| PluginManagerError::PipeError("Failed to capture stderr".to_string()))?;

    // Keep both pipes drained for the plugin lifetime while retaining only a bounded diagnostic tail.
    let stdout_handle = tokio::spawn(Self::read_stream_tail(stdout));
    let stderr_handle = tokio::spawn(Self::read_stream_tail(stderr));

    let startup = timeout(PLUGIN_START_TIMEOUT, async {
      let client = PluginClient::connect(&socket_name)
        .await
        .map_err(|error| PluginManagerError::ConnectionError(error.to_string()))?;
      client
        .handshake()
        .await
        .map_err(|error| PluginManagerError::StartError(error.to_string()))?;
      let schema = client
        .get_schema()
        .await
        .map_err(|error| PluginManagerError::StartError(error.to_string()))?;

      Ok::<_, PluginManagerError>((client, schema))
    })
    .await;

    let (client, schema) = match startup {
      Ok(Ok(result)) => result,
      Ok(Err(error)) => {
        let diagnostics = Self::cleanup_failed_start(&mut process, &socket_path, stdout_handle, stderr_handle).await;
        return Err(Self::with_startup_diagnostics(error, diagnostics));
      },
      Err(_) => {
        let diagnostics = Self::cleanup_failed_start(&mut process, &socket_path, stdout_handle, stderr_handle).await;
        return Err(Self::with_startup_diagnostics(
          PluginManagerError::ConnectionError("Plugin startup timeout".to_string()),
          diagnostics,
        ));
      },
    };

    let registration = match PluginRegistration::new(plugin_name.clone(), schema.clone()) {
      Ok(registration) => registration,
      Err(error) => {
        drop(client);
        let diagnostics = Self::cleanup_failed_start(&mut process, &socket_path, stdout_handle, stderr_handle).await;
        return Err(Self::with_startup_diagnostics(
          PluginManagerError::StartError(format!("invalid plugin schema: {error}")),
          diagnostics,
        ));
      },
    };
    if let Err(error) = self.plugin_registry.lock().await.register(registration) {
      drop(client);
      let _ = Self::cleanup_failed_start(&mut process, &socket_path, stdout_handle, stderr_handle).await;
      return Err(error);
    }

    self.active_plugins.lock().await.insert(
      plugin_name.clone(),
      PluginInstance {
        process,
        socket_path,
        client,
      },
    );

    Ok(schema)
  }

  pub async fn get_schema_keys(&self) -> HashMap<String, String> {
    self
      .plugin_registry
      .lock()
      .await
      .keys
      .iter()
      .map(|(key, registration)| (key.clone(), registration.plugin_name.clone()))
      .collect()
  }

  /// Resolves a task type in constant time and shares its compiled validator.
  pub async fn resolve_key(&self, key: &str) -> Option<PluginRegistration> {
    self.plugin_registry.lock().await.keys.get(key).cloned()
  }

  /// Resolves an execution capability independently of a plugin's task key.
  pub async fn resolve_capability(&self, capability: &str) -> Option<PluginRegistration> {
    self.plugin_registry.lock().await.capabilities.get(capability).cloned()
  }

  /// Clones a connected client without holding the manager lifecycle lock during execution.
  pub async fn get_client(&self, plugin_name: &str) -> Result<PluginClient> {
    let active_plugins = self.active_plugins.lock().await;

    if let Some(instance) = active_plugins.get(plugin_name) {
      Ok(instance.client.clone())
    } else {
      Err(PluginManagerError::PluginNotFound(plugin_name.to_string()))
    }
  }

  /// Shutdown a specific plugin
  pub async fn shutdown_plugin(&self, plugin_name: &str) -> Result<()> {
    let mut active_plugins = self.active_plugins.lock().await;
    let mut instance = active_plugins
      .remove(plugin_name)
      .ok_or_else(|| PluginManagerError::PluginNotFound(plugin_name.to_string()))?;

    let mut registry = self.plugin_registry.lock().await;
    registry.remove_plugin(plugin_name);
    drop(registry);
    drop(active_plugins);

    // Handle the client shutdown
    let shutdown_result = instance
      .client
      .shutdown()
      .await
      .map_err(|error| PluginManagerError::ConnectionError(error.to_string()));

    match tokio::time::timeout(Duration::from_secs(1), instance.process.wait()).await {
      Ok(Ok(_)) => {},
      Ok(Err(e)) => {
        // An error occurred while waiting for the process
        return Err(PluginManagerError::ShutdownError(format!(
          "Error while waiting for process: {}",
          e
        )));
      },
      Err(_) => {
        // Timeout expired try to kill process
        match instance.process.kill().await {
          Ok(_) => {
            let _ = tokio::fs::remove_file(&instance.socket_path).await;
          },
          Err(error) => {
            return Err(PluginManagerError::ShutdownError(format!(
              "Failed to kill process: {error}"
            )));
          },
        }
      },
    }

    // Return the shutdown result
    shutdown_result
  }

  /// Shutdown all plugins
  pub async fn shutdown_all(&self) -> Vec<Result<()>> {
    let active_plugins = self.active_plugins.lock().await;
    let plugin_names: Vec<String> = active_plugins.keys().cloned().collect();
    drop(active_plugins);

    let mut results = Vec::new();
    for plugin_name in plugin_names {
      results.push(self.shutdown_plugin(&plugin_name).await);
    }
    results
  }

  /// List all active plugins
  pub async fn list_active_plugins(&self) -> Vec<String> {
    let active_plugins = self.active_plugins.lock().await;
    active_plugins.keys().cloned().collect()
  }

  /// Check if a plugin is running
  pub async fn is_plugin_running(&self, plugin_name: &str) -> bool {
    let active_plugins = self.active_plugins.lock().await;
    active_plugins.contains_key(plugin_name)
  }

  /// Get socket path for a running plugin
  pub async fn get_socket_path(&self, plugin_name: &str) -> Option<OsString> {
    let active_plugins = self.active_plugins.lock().await;
    active_plugins
      .get(plugin_name)
      .map(|instance| instance.socket_path.clone())
  }

  pub fn cleanup(&mut self) {
    if let Ok(mut plugins) = self.active_plugins.try_lock() {
      for (_, mut instance) in plugins.drain() {
        let _ = instance.process.start_kill();
        if let Some(path) = instance.socket_path.to_str() {
          let _ = std::fs::remove_file(path);
        }
      }
    }
  }
}

impl Drop for PluginManager {
  fn drop(&mut self) {
    self.cleanup();
  }
}

#[cfg(test)]
mod tests {
  use std::collections::HashSet;

  use async_trait::async_trait;

  use super::*;
  use crate::plugin_client::PluginExecutionRequest;
  use octa_plugin::protocol::PluginResponse;
  use tempfile::TempDir;
  use tokio::fs;
  use tokio::time::Duration;
  use tokio_util::sync::CancellationToken;

  const TEST_TIMEOUT: Duration = Duration::from_secs(5);

  #[derive(Default)]
  struct RecordingLauncher {
    request: StdMutex<Option<PluginLaunchRequest>>,
  }

  #[async_trait]
  impl PluginLauncher for RecordingLauncher {
    async fn launch(&self, request: PluginLaunchRequest) -> std::result::Result<PluginProcess, PluginLaunchError> {
      *self.request.lock().unwrap() = Some(request);
      Err(PluginLaunchError::Io(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "launch rejected by test",
      )))
    }
  }

  struct TestSetup {
    plugin_manager: PluginManager,
    plugin_path: PathBuf,
  }

  impl TestSetup {
    async fn new(plugins_dir: PathBuf, plugin_name: &str) -> Self {
      let plugin_path = plugins_dir.join(plugin_name);
      let plugin_manager = PluginManager::new(plugins_dir);

      Self {
        plugin_manager,
        plugin_path,
      }
    }

    fn plugin_name(&self) -> &str {
      self.plugin_path.file_name().unwrap().to_str().unwrap()
    }
  }

  #[test]
  fn test_socket_paths_are_unique() {
    let manager = PluginManager::new(".");
    let paths = (0..1_000)
      .map(|_| manager.generate_socket_path())
      .collect::<HashSet<_>>();

    assert_eq!(paths.len(), 1_000);
  }

  #[tokio::test]
  async fn delegates_process_creation_to_the_configured_launcher() {
    let directory = TempDir::new().unwrap();
    let workspace = directory.path().join("workspace");
    let executable = directory.path().join("custom-plugin");
    fs::write(&executable, b"plugin").await.unwrap();
    let launcher = Arc::new(RecordingLauncher::default());
    let manager = PluginManager::with_launcher(directory.path(), &workspace, launcher.clone());

    let error = manager.start_plugin("custom-plugin").await.unwrap_err();

    assert!(matches!(error, PluginManagerError::Launch(PluginLaunchError::Io(_))));
    let request = launcher.request.lock().unwrap().take().unwrap();
    assert_eq!(request.executable, executable);
    assert!(request.arguments.is_empty());
    assert!(request.environment.is_empty());
    assert_eq!(request.workspace, workspace);
    assert!(request
      .socket_path
      .file_name()
      .unwrap()
      .to_string_lossy()
      .starts_with("octa."));
  }

  #[test]
  fn resolves_script_interpreters_before_launch() {
    let python = PathBuf::from("plugin.py");
    let (executable, arguments) = PluginManager::resolve_plugin_command(&python);
    assert_eq!(executable, PathBuf::from("python3"));
    assert_eq!(arguments, [python.into_os_string()]);

    let native = PathBuf::from("octa_plugin_shell");
    let (executable, arguments) = PluginManager::resolve_plugin_command(&native);
    assert_eq!(executable, native);
    assert!(arguments.is_empty());
  }

  #[tokio::test]
  async fn retains_only_the_tail_of_plugin_diagnostics() {
    let (mut writer, reader) = tokio::io::duplex(STARTUP_DIAGNOSTIC_LIMIT * 2);
    let mut input = vec![b'a'; STARTUP_DIAGNOSTIC_LIMIT];
    input.extend(vec![b'b'; STARTUP_DIAGNOSTIC_LIMIT]);
    let write = tokio::spawn(async move {
      tokio::io::AsyncWriteExt::write_all(&mut writer, &input).await.unwrap();
    });

    let output = PluginManager::read_stream_tail(reader).await.unwrap();

    write.await.unwrap();
    assert!(output.starts_with("[earlier plugin output truncated]\n"));
    let tail = output.split_once('\n').unwrap().1.as_bytes();
    assert_eq!(tail.len(), STARTUP_DIAGNOSTIC_LIMIT);
    assert!(tail.iter().all(|byte| *byte == b'b'));
  }

  #[tokio::test]
  async fn preserves_short_plugin_diagnostics_without_a_marker() {
    let (mut writer, reader) = tokio::io::duplex(16);
    tokio::io::AsyncWriteExt::write_all(&mut writer, b"diagnostic")
      .await
      .unwrap();
    drop(writer);

    assert_eq!(PluginManager::read_stream_tail(reader).await.unwrap(), "diagnostic");
  }

  #[test]
  fn startup_diagnostics_are_returned_without_changing_error_kind() {
    let error = PluginManager::with_startup_diagnostics(
      PluginManagerError::ConnectionError("connection closed".to_owned()),
      "\nPlugin stderr: crash details".to_owned(),
    );

    assert!(matches!(
      error,
      PluginManagerError::ConnectionError(message)
        if message == "connection closed\nPlugin stderr: crash details"
    ));

    let error = PluginManager::with_startup_diagnostics(
      PluginManagerError::StartError("invalid handshake".to_owned()),
      "\nPlugin stdout: details".to_owned(),
    );
    assert!(matches!(
      error,
      PluginManagerError::StartError(message) if message == "invalid handshake\nPlugin stdout: details"
    ));

    let error = PluginManager::with_startup_diagnostics(
      PluginManagerError::PluginNotFound("missing".to_owned()),
      "\nPlugin stderr: ignored".to_owned(),
    );
    assert!(matches!(error, PluginManagerError::PluginNotFound(name) if name == "missing"));
  }

  #[test]
  fn plugin_registry_rejects_duplicate_keys_and_capabilities() {
    let registration = |plugin: &str, key: &str, capabilities: &[&str]| {
      PluginRegistration::new(
        plugin.to_owned(),
        Schema {
          key: key.to_owned(),
          supports_raw: false,
          capabilities: capabilities.iter().map(|value| (*value).to_owned()).collect(),
          validation_schema: None,
        },
      )
      .unwrap()
    };
    let mut registry = PluginRegistry::default();

    registry.register(registration("first", "first", &["shell"])).unwrap();
    assert_eq!(registry.capabilities["shell"].plugin_name(), "first");
    assert!(matches!(
      registry.register(registration("second", "first", &[])),
      Err(PluginManagerError::PluginSelectorAlreadyRegistered(value)) if value == "first"
    ));
    assert!(matches!(
      registry.register(registration("second", "second", &["shell"])),
      Err(PluginManagerError::PluginSelectorAlreadyRegistered(value)) if value == "shell"
    ));
  }

  #[test]
  fn plugin_validation_schema_is_compiled_once() {
    let registration = PluginRegistration::new(
      "plugin".to_owned(),
      Schema {
        key: "key".to_owned(),
        supports_raw: false,
        capabilities: Vec::new(),
        validation_schema: serde_json::json!({ "type": "string" }).as_object().cloned(),
      },
    )
    .unwrap();

    registration.validate(&serde_json::json!("valid")).unwrap();
    let cloned = registration.clone();
    assert!(registration.validate(&serde_json::json!(1)).is_err());
    assert!(Arc::ptr_eq(
      registration.validator.as_ref().unwrap(),
      cloned.validator.as_ref().unwrap()
    ));
  }

  #[tokio::test]
  async fn test_parallel_managers_start_same_plugin() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let managers = [
      PluginManager::new(plugins_dir.clone()),
      PluginManager::new(plugins_dir.clone()),
      PluginManager::new(plugins_dir),
    ];

    let starts = tokio::time::timeout(Duration::from_secs(10), async {
      tokio::join!(
        managers[0].start_plugin("test.py"),
        managers[1].start_plugin("test.py"),
        managers[2].start_plugin("test.py")
      )
    })
    .await
    .expect("parallel plugin startup timed out");

    assert!(starts.0.is_ok());
    assert!(starts.1.is_ok());
    assert!(starts.2.is_ok());

    for manager in &managers {
      assert!(manager.shutdown_all().await.iter().all(Result::is_ok));
    }
  }

  #[tokio::test]
  async fn test_start_plugin() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    let result = tokio::time::timeout(TEST_TIMEOUT, setup.plugin_manager.start_plugin(plugin_name)).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_ok());

    assert!(setup.plugin_manager.is_plugin_running("test").await);
  }

  #[tokio::test]
  async fn test_plugin_execution() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    // Start plugin
    if let Err(e) = setup.plugin_manager.start_plugin(plugin_name).await {
      println!("Error start plugin. Err: {}", e);
    }

    // Get client
    let client = match setup.plugin_manager.get_client("test").await {
      Ok(client) => client,
      Err(e) => return println!("Can't get client, Err: {}", e),
    };

    // Execute command
    let cancel_token = CancellationToken::new();
    let mut execution = client
      .start_execution(
        PluginExecutionRequest {
          params: "test".to_owned(),
          dry: false,
          args: Vec::new(),
          dir: PathBuf::from("."),
          vars: HashMap::new(),
          envs: HashMap::new(),
          secret_vars: Vec::new(),
          redact_params: false,
          raw: false,
        },
        cancel_token.clone(),
      )
      .await
      .unwrap();

    let execution_id = execution.id().to_owned();
    assert!(Uuid::parse_str(&execution_id).is_ok());

    // Receive output
    let mut received_stdout = false;
    let mut received_exit = false;

    while let Ok(Some(response)) = execution.receive_output(&cancel_token).await {
      match response {
        PluginResponse::Stdout { id, line } => {
          assert_eq!(id, execution_id);
          assert_eq!(line, "test output");
          received_stdout = true;
        },
        PluginResponse::ExitStatus { id, code } => {
          assert_eq!(id, execution_id);
          assert_eq!(code, 0);
          received_exit = true;
          break;
        },
        _ => {},
      }
    }

    assert!(received_stdout, "Did not receive expected stdout");
    assert!(received_exit, "Did not receive exit status");
  }

  #[tokio::test]
  async fn test_shutdown_plugin() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    // Start plugin
    setup.plugin_manager.start_plugin(plugin_name).await.unwrap();

    // Shutdown plugin
    let result = tokio::time::timeout(TEST_TIMEOUT, setup.plugin_manager.shutdown_plugin("test")).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_ok());

    // Verify plugin is not running
    assert!(!setup.plugin_manager.is_plugin_running("test").await);
  }

  #[tokio::test]
  async fn test_start_nonexistent_plugin() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;

    let result = setup.plugin_manager.start_plugin("nonexistent_plugin").await;
    assert!(matches!(result, Err(PluginManagerError::PluginNotFound(_))));
  }

  #[tokio::test]
  async fn test_start_duplicate_plugin() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    // Start the plugin first time
    setup.plugin_manager.start_plugin(plugin_name).await.unwrap();

    // Try to start it again
    let result = setup.plugin_manager.start_plugin(plugin_name).await;
    assert!(matches!(result, Err(PluginManagerError::PluginAlreadyRunning(_))));
  }

  #[tokio::test]
  async fn test_shutdown_all() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    // Start plugin
    setup.plugin_manager.start_plugin(plugin_name).await.unwrap();

    // Shutdown all plugins
    let results = setup.plugin_manager.shutdown_all().await;
    assert!(results.iter().all(|r| r.is_ok()));

    // Verify no plugins are running
    assert!(setup.plugin_manager.list_active_plugins().await.is_empty());
  }

  #[tokio::test]
  async fn test_list_plugins_empty() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;

    // Initially should be empty
    let plugins = setup.plugin_manager.list_active_plugins().await;
    assert!(plugins.is_empty());
  }

  #[tokio::test]
  async fn test_list_single_plugin() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    // Start plugin
    setup.plugin_manager.start_plugin(plugin_name).await.unwrap();

    // Check list
    let plugins = setup.plugin_manager.list_active_plugins().await;
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0], "test");
  }

  #[tokio::test]
  async fn test_list_plugins_after_shutdown() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    // Start plugin
    setup.plugin_manager.start_plugin(plugin_name).await.unwrap();

    // Verify plugin is listed
    let plugins = setup.plugin_manager.list_active_plugins().await;
    assert_eq!(plugins.len(), 1);
    assert_eq!(plugins[0], "test");

    // Shutdown plugin
    setup.plugin_manager.shutdown_plugin("test").await.unwrap();

    // Verify list is empty
    let plugins = setup.plugin_manager.list_active_plugins().await;
    assert!(plugins.is_empty());
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn test_starts_and_lists_multiple_plugins_in_parallel() {
    let source = std::fs::read_to_string(PathBuf::from("../../plugins/test.py")).unwrap();
    let plugins_dir = TempDir::new().unwrap();
    let setup = TestSetup::new(plugins_dir.path().to_path_buf(), "test_1.py").await;
    let plugin_names = ["test_1.py", "test_2.py", "test_3.py"];

    for (index, name) in plugin_names.iter().enumerate() {
      let plugin = source.replace(r#""key": "key""#, &format!(r#""key": "key-{index}""#));
      std::fs::write(plugins_dir.path().join(name), plugin).unwrap();
    }
    let starts = tokio::join!(
      setup.plugin_manager.start_plugin(plugin_names[0]),
      setup.plugin_manager.start_plugin(plugin_names[1]),
      setup.plugin_manager.start_plugin(plugin_names[2]),
    );
    assert!(starts.0.is_ok());
    assert!(starts.1.is_ok());
    assert!(starts.2.is_ok());

    // Check list
    let mut plugins = setup.plugin_manager.list_active_plugins().await;
    plugins.sort(); // Sort for consistent comparison

    assert_eq!(plugins.len(), 3);

    assert_eq!(plugins, vec!["test_1", "test_2", "test_3"]);

    assert!(setup.plugin_manager.shutdown_all().await.iter().all(Result::is_ok));
  }

  #[tokio::test]
  async fn test_list_plugins_concurrent_access() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    // Create Arc for plugin manager to share across threads
    let manager = Arc::new(setup.plugin_manager);

    // Start plugin
    manager.start_plugin(plugin_name).await.unwrap();

    // Spawn multiple concurrent list operations
    let handles: Vec<_> = (0..10)
      .map(|_| {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move { manager.list_active_plugins().await })
      })
      .collect();

    // All should complete successfully
    for handle in handles {
      let plugins = handle.await.unwrap();
      assert_eq!(plugins.len(), 1);
      assert_eq!(plugins[0], "test");
    }
  }

  #[tokio::test]
  async fn test_plugin_restart() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_name();

    // Start plugin
    setup.plugin_manager.start_plugin(plugin_name).await.unwrap();

    // Shutdown plugin
    setup.plugin_manager.shutdown_plugin("test").await.unwrap();

    // Wait a bit to ensure cleanup
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Start plugin again
    let result = setup.plugin_manager.start_plugin(plugin_name).await;
    assert!(result.is_ok());
  }

  #[tokio::test]
  async fn test_plugin_execution_timeout() {
    let temp_dir = TempDir::new().unwrap();

    // Create a plugin path that doesn't respond
    let hanging_plugin = temp_dir.path().join("hanging.py");
    fs::write(
      &hanging_plugin,
      r#"
import time
time.sleep(10)  # Simulate a hanging plugin
    "#,
    )
    .await
    .unwrap();

    let setup = TestSetup::new(temp_dir.keep(), "hanging.py").await;

    // Try to start the hanging plugin
    let result = tokio::time::timeout(
      Duration::from_secs(10),
      setup
        .plugin_manager
        .start_plugin(hanging_plugin.file_name().unwrap().to_str().unwrap()),
    )
    .await
    .expect("plugin startup exceeded its internal timeout");

    assert!(matches!(result, Err(PluginManagerError::ConnectionError(_))));
  }

  #[tokio::test]
  async fn test_invalid_plugin_operations() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;

    // Try to get client for non-existent plugin
    let result = setup.plugin_manager.get_client("nonexistent").await;
    assert!(matches!(result, Err(PluginManagerError::PluginNotFound(_))));

    // Try to shutdown non-existent plugin
    let result = setup.plugin_manager.shutdown_plugin("nonexistent").await;
    assert!(matches!(result, Err(PluginManagerError::PluginNotFound(_))));

    // Try to get socket path for non-existent plugin
    let result = setup.plugin_manager.get_socket_path("nonexistent").await;
    assert!(result.is_none());
  }

  #[tokio::test]
  async fn test_concurrent_plugin_operations() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_name().to_string();
    let manager = Arc::new(setup.plugin_manager);

    // Start plugin
    manager.start_plugin(&plugin_name).await.unwrap();

    // Spawn multiple concurrent operations
    let handles: Vec<_> = (0..5)
      .map(|i| {
        let manager = Arc::clone(&manager);
        tokio::spawn(async move {
          match i % 3 {
            0 => manager.list_active_plugins().await,
            1 => {
              manager.get_client("test").await.unwrap();
              vec![]
            },
            _ => {
              manager.get_socket_path("test").await;
              vec![]
            },
          }
        })
      })
      .collect();

    // All operations should complete without errors
    for handle in handles {
      handle.await.unwrap();
    }
  }

  #[tokio::test]
  async fn test_plugin_cleanup() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_name();

    // Start plugin
    setup.plugin_manager.start_plugin(plugin_name).await.unwrap();

    // Get socket path before cleanup
    #[cfg(unix)]
    let socket_path = setup
      .plugin_manager
      .get_socket_path("test")
      .await
      .expect("Failed to get socket path");

    // Verify socket file exists
    #[cfg(unix)]
    assert!(tokio::fs::metadata(&socket_path).await.is_ok());

    // Drop plugin manager to trigger cleanup
    drop(setup.plugin_manager);

    // Verify socket file is removed
    #[cfg(unix)]
    assert!(tokio::fs::metadata(&socket_path).await.is_err());
  }

  #[tokio::test]
  async fn test_plugin_crash_during_startup() {
    let temp_dir = TempDir::new().unwrap();

    // Create a plugin that crashes immediately
    let crashing_plugin = temp_dir.path().join("crash.py");
    fs::write(
      &crashing_plugin,
      r#"
import sys
print("startup stdout", flush=True)
print("startup stderr", file=sys.stderr, flush=True)
sys.exit(1)  # Crash immediately
      "#,
    )
    .await
    .unwrap();

    // Make the plugin executable on Unix systems
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(&crashing_plugin, std::fs::Permissions::from_mode(0o755))
        .await
        .unwrap();
    }

    let setup = TestSetup::new(temp_dir.keep(), "crash.py").await;

    let result = setup
      .plugin_manager
      .start_plugin(crashing_plugin.file_name().unwrap().to_str().unwrap())
      .await;

    assert!(matches!(
      result,
      Err(PluginManagerError::ConnectionError(message))
        if message.contains("Plugin stdout: startup stdout")
          && message.contains("Plugin stderr: startup stderr")
    ));
  }
}
