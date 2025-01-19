use octa_plugin::protocol::Schema;
use octa_plugin::socket::{interpret_local_socket_name, make_local_socket_name};
use std::ffi::OsString;
use std::{
  collections::HashMap,
  hash::{Hash, Hasher},
  path::PathBuf,
  process::Stdio,
  sync::Arc,
  time::SystemTime,
};
use thiserror::Error;
use tokio::io::{self, AsyncReadExt};
use tokio::process::Command;
use tokio::{
  process::Child,
  sync::Mutex,
  time::{timeout, Duration},
};

use crate::plugin_client::{PluginClient, PluginClientError};

#[derive(Error, Debug)]
pub enum PluginManagerError {
  #[error("Plugin not found: {0}")]
  PluginNotFound(String),

  #[error("Plugin client not found: {0}")]
  PluginClientNotFound(String),

  #[error("Plugin already running: {0}")]
  PluginAlreadyRunning(String),

  #[error("Failed to start plugin: {0}")]
  StartError(String),

  #[error("Failed to shutdown plugin: {0}")]
  ShutdownError(String),

  #[error("Plugin connection error: {0}")]
  ConnectionError(String),

  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),

  #[error("Socket path error: {0}")]
  SocketPath(String),

  #[error("Pipe error: {0}")]
  PipeError(String),
}

type Result<T> = std::result::Result<T, PluginManagerError>;

#[derive(Debug)]
struct PluginInstance {
  process: Child,
  socket_path: OsString,
  client: Arc<Mutex<Option<PluginClient>>>,
}

pub struct PluginManager {
  plugins_dir: PathBuf,
  active_plugins: Arc<Mutex<HashMap<String, PluginInstance>>>,
  plugins_schema: Arc<Mutex<HashMap<String, Schema>>>,
}

impl PluginManager {
  pub fn new(plugins_dir: impl Into<PathBuf>) -> Self {
    Self {
      plugins_dir: plugins_dir.into(),
      active_plugins: Arc::new(Mutex::new(HashMap::new())),
      plugins_schema: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  /// Generate a unique socket path for a plugin
  fn generate_socket_path(&self, plugin_name: &str) -> OsString {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    plugin_name.hash(&mut hasher);
    SystemTime::now().hash(&mut hasher);
    let unique_id = format!("{:016x}", hasher.finish());

    make_local_socket_name(&unique_id)
  }

  async fn read_stream_to_string<R>(mut stream: R) -> io::Result<String>
  where
    R: tokio::io::AsyncRead + Unpin,
  {
    let mut buffer = String::new();
    stream.read_to_string(&mut buffer).await?;
    Ok(buffer)
  }

  /// Start a plugin and establish connection
  pub async fn start_plugin(&self, plugin_name: &str) -> Result<Schema> {
    let mut active_plugins = self.active_plugins.lock().await;

    if active_plugins.contains_key(plugin_name) {
      return Err(PluginManagerError::PluginAlreadyRunning(plugin_name.to_string()));
    }

    let socket_path = self.generate_socket_path(plugin_name);
    let plugin_path = self.plugins_dir.join(plugin_name);

    if !plugin_path.exists() {
      return Err(PluginManagerError::PluginNotFound(
        plugin_path.to_string_lossy().to_string(),
      ));
    }

    let mut command = self.setup_command(plugin_path, socket_path.clone());
    let mut process = command.spawn()?;

    let stdout = process
      .stdout
      .take()
      .ok_or_else(|| PluginManagerError::PipeError("Failed to capture stdout".to_string()))?;
    let stderr = process
      .stderr
      .take()
      .ok_or_else(|| PluginManagerError::PipeError("Failed to capture stderr".to_string()))?;

    // Spawn tasks to collect stdout and stderr
    // We need it if we get error
    let stdout_handle = tokio::spawn(Self::read_stream_to_string(stdout));
    let stderr_handle = tokio::spawn(Self::read_stream_to_string(stderr));

    // Convert socket path to proper format for connection
    let socket_name =
      interpret_local_socket_name(&socket_path).map_err(|e| PluginManagerError::SocketPath(e.to_string()))?;

    // Try to connect with timeout
    let client = match timeout(Duration::from_secs(5), async {
      loop {
        match PluginClient::connect(&socket_name).await {
          Ok(client) => return Ok::<_, PluginClientError>(client),
          Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
      }
    })
    .await
    {
      Ok(Ok(client)) => client,
      Err(_) => {
        let stdout_content = stdout_handle
          .await
          .map_err(|e| PluginManagerError::StartError(e.to_string()))?
          .map_err(|e| PluginManagerError::StartError(e.to_string()))?;

        let stderr_content = stderr_handle
          .await
          .map_err(|e| PluginManagerError::StartError(e.to_string()))?
          .map_err(|e| PluginManagerError::StartError(e.to_string()))?;

        println!("Plugin stdout: {}", stdout_content);
        println!("Plugin stderr: {}", stderr_content);

        return Err(PluginManagerError::ConnectionError("Connection timeout".to_string()));
      },
      Ok(Err(e)) => return Err(PluginManagerError::ConnectionError(e.to_string())),
    };

    let client = Arc::new(Mutex::new(Some(client)));

    active_plugins.insert(
      plugin_name.to_string(),
      PluginInstance {
        process,
        socket_path,
        client: client.clone(),
      },
    );

    let schema = {
      let mut client_guard = client.lock().await;
      if let Some(ref mut client) = *client_guard {
        client
          .handshake()
          .await
          .map_err(|e| PluginManagerError::StartError(e.to_string()))?;
        client
          .get_schema()
          .await
          .map_err(|e| PluginManagerError::StartError(e.to_string()))?
      } else {
        return Err(PluginManagerError::StartError("Client not available".to_string()));
      }
    };

    {
      let mut schemas = self.plugins_schema.lock().await;
      schemas.insert(plugin_name.to_owned(), schema.clone());
    }

    Ok(schema)
  }

  pub async fn get_schema_keys(&self) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let schemas = self.plugins_schema.lock().await;

    for (plugin_name, schema) in schemas.iter() {
      result.insert(schema.key.clone(), plugin_name.clone());
    }

    result
  }

  /// Platform-specific command setup for Unix
  #[cfg(not(windows))]
  fn setup_command(&self, plugin_path: PathBuf, socket_path: OsString) -> Command {
    let mut command = self.get_command(plugin_path);

    command
      .args(["--socket-path", socket_path.to_str().unwrap()])
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .process_group(0);
    command
  }

  /// Platform-specific command setup for Windows
  #[cfg(windows)]
  fn setup_command(&self, plugin_path: PathBuf, socket_path: OsString) -> Command {
    #[allow(unused_imports)]
    use std::os::windows::process::CommandExt;

    const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    const DETACHED_PROCESS: u32 = 0x00000008;

    let mut command = self.get_command(plugin_path);
    command
      .args(["--socket-path", socket_path.to_str().unwrap()])
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true)
      .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | DETACHED_PROCESS);
    command
  }

  fn get_command(&self, plugin_path: PathBuf) -> Command {
    match plugin_path.extension().and_then(|e| e.to_str()) {
      Some("py") => {
        let mut cmd = Command::new("python3");
        cmd.arg(plugin_path);
        cmd
      },
      _ => Command::new(plugin_path),
    }
  }

  /// Get a reference to a plugin client
  pub async fn get_client(&self, plugin_name: &str) -> Result<Arc<Mutex<Option<PluginClient>>>> {
    let active_plugins = self.active_plugins.lock().await;

    if let Some(instance) = active_plugins.get(plugin_name) {
      Ok(Arc::clone(&instance.client))
    } else {
      Err(PluginManagerError::PluginNotFound(plugin_name.to_string()))
    }
  }

  /// Shutdown a specific plugin
  pub async fn shutdown_plugin(&self, plugin_name: &str) -> Result<()> {
    {
      let mut schemas = self.plugins_schema.lock().await;
      schemas.remove(plugin_name);
    }

    // First get the instance
    let mut instance = {
      let mut active_plugins = self.active_plugins.lock().await;
      active_plugins
        .remove(plugin_name)
        .ok_or_else(|| PluginManagerError::PluginNotFound(plugin_name.to_string()))?
    };

    // Handle the client shutdown
    let shutdown_result = {
      // TODO: add timeout to shutdown plugin
      let mut client_guard = instance.client.lock().await;
      if let Some(client) = client_guard.as_mut() {
        let res = client
          .shutdown()
          .await
          .map_err(|e| PluginManagerError::ConnectionError(e.to_string()));

        // Drop client to awoid panic in library
        *client_guard = None;

        res
      } else {
        Err(PluginManagerError::PluginClientNotFound(plugin_name.to_string()))
      }
    };

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
          Err(e) => eprintln!("Failed to kill process: {}", e),
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
  use super::*;
  use octa_plugin::protocol::ServerResponse;
  use tempfile::TempDir;
  use tokio::fs;
  use tokio::time::Duration;
  use tokio_util::sync::CancellationToken;

  const TEST_TIMEOUT: Duration = Duration::from_secs(5);

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

  #[tokio::test]
  async fn test_start_plugin() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    let result = tokio::time::timeout(TEST_TIMEOUT, setup.plugin_manager.start_plugin(plugin_name)).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_ok());

    assert!(setup.plugin_manager.is_plugin_running(plugin_name).await);
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
    let client = match setup.plugin_manager.get_client(plugin_name).await {
      Ok(client) => client,
      Err(e) => return println!("Can't get client, Err: {}", e),
    };
    let mut client_guard = client.lock().await;
    let client = client_guard.as_mut().unwrap();

    // Execute command
    let cancel_token = CancellationToken::new();
    let execution_id = client
      .execute(
        "test".to_string(),
        vec![],
        PathBuf::from("."),
        HashMap::new(),
        HashMap::new(),
        cancel_token.clone(),
      )
      .await
      .unwrap();

    assert_eq!(execution_id, "test-execution-id");

    // Receive output
    let mut received_stdout = false;
    let mut received_exit = false;

    while let Ok(Some(response)) = client.receive_output(&cancel_token).await {
      match response {
        ServerResponse::Stdout { id, line } => {
          assert_eq!(id, "test-execution-id");
          assert_eq!(line, "test output");
          received_stdout = true;
        },
        ServerResponse::ExitStatus { id, code } => {
          assert_eq!(id, "test-execution-id");
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
    let result = tokio::time::timeout(TEST_TIMEOUT, setup.plugin_manager.shutdown_plugin(plugin_name)).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_ok());

    // Verify plugin is not running
    assert!(!setup.plugin_manager.is_plugin_running(plugin_name).await);
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
    assert_eq!(plugins[0], plugin_name);
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
    assert_eq!(plugins[0], plugin_name);

    // Shutdown plugin
    setup.plugin_manager.shutdown_plugin(plugin_name).await.unwrap();

    // Verify list is empty
    let plugins = setup.plugin_manager.list_active_plugins().await;
    assert!(plugins.is_empty());
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn test_list_multiple_plugins() {
    let plugins_dir = PathBuf::from("../../plugins").canonicalize().unwrap();
    let setup = TestSetup::new(plugins_dir, "test.py").await;
    let plugin_name = setup.plugin_path.file_name().unwrap().to_str().unwrap();

    // Start same plugin multiple times with different names
    // In real scenario, you'd have different plugins
    let plugin_names = vec![
      format!("{}_1", plugin_name),
      format!("{}_2", plugin_name),
      format!("{}_3", plugin_name),
    ];

    // Create symbolic links to the test plugin with different names
    for name in &plugin_names {
      std::os::unix::fs::symlink(&setup.plugin_path, setup.plugin_path.parent().unwrap().join(name)).unwrap();
      setup.plugin_manager.start_plugin(name).await.unwrap();
    }

    // Check list
    let mut plugins = setup.plugin_manager.list_active_plugins().await;
    plugins.sort(); // Sort for consistent comparison

    assert_eq!(plugins.len(), 3);

    let mut expected = plugin_names.clone();
    expected.sort();
    assert_eq!(plugins, expected);

    // Cleanup symbolic links
    for name in &plugin_names {
      let _ = std::fs::remove_file(setup.plugin_path.parent().unwrap().join(name));
    }
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
      assert_eq!(plugins[0], plugin_name);
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
    setup.plugin_manager.shutdown_plugin(plugin_name).await.unwrap();

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

    let setup = TestSetup::new(temp_dir.into_path(), "hanging.py").await;

    // Try to start the hanging plugin
    let result = tokio::time::timeout(
      Duration::from_secs(2),
      setup
        .plugin_manager
        .start_plugin(hanging_plugin.file_name().unwrap().to_str().unwrap()),
    )
    .await;

    assert!(result.is_err() || result.unwrap().is_err());
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
        let plugin_name = plugin_name.to_string();
        tokio::spawn(async move {
          match i % 3 {
            0 => manager.list_active_plugins().await,
            1 => {
              manager.get_client(&plugin_name).await.unwrap();
              vec![]
            },
            _ => {
              manager.get_socket_path(&plugin_name).await;
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
    let socket_path = setup
      .plugin_manager
      .get_socket_path(plugin_name)
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

    let setup = TestSetup::new(temp_dir.into_path(), "crash.py").await;

    let result = setup
      .plugin_manager
      .start_plugin(crashing_plugin.file_name().unwrap().to_str().unwrap())
      .await;

    assert!(matches!(result, Err(PluginManagerError::ConnectionError(_))));
  }
}
