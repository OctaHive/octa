use interprocess::local_socket::{GenericFilePath, ToFsName};
use octa_plugin::socket::make_local_socket_name;
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

  #[error("Plugin already running: {0}")]
  PluginAlreadyRunning(String),

  #[error("Failed to start plugin: {0}")]
  StartError(String),

  #[error("Plugin connection error: {0}")]
  ConnectionError(String),

  #[error("IO error: {0}")]
  Io(#[from] std::io::Error),

  #[error("Socket path error: {0}")]
  SocketPath(String),
}

type Result<T> = std::result::Result<T, PluginManagerError>;

#[derive(Debug)]
struct PluginInstance {
  process: Child,
  socket_path: OsString,
  client: Arc<Mutex<PluginClient>>,
}

pub struct PluginManager {
  plugins_dir: PathBuf,
  active_plugins: Arc<Mutex<HashMap<String, PluginInstance>>>,
}

impl PluginManager {
  pub fn new(plugins_dir: impl Into<PathBuf>) -> Self {
    Self {
      plugins_dir: plugins_dir.into(),
      active_plugins: Arc::new(Mutex::new(HashMap::new())),
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

  /// Start a plugin and establish connection
  pub async fn start_plugin(&self, plugin_name: &str) -> Result<()> {
    let mut active_plugins = self.active_plugins.lock().await;

    if active_plugins.contains_key(plugin_name) {
      return Err(PluginManagerError::PluginAlreadyRunning(plugin_name.to_string()));
    }

    let socket_path = self.generate_socket_path(plugin_name);
    let plugin_path = self.plugins_dir.join(plugin_name);

    if !plugin_path.exists() {
      return Err(PluginManagerError::PluginNotFound(plugin_name.to_string()));
    }

    // Start plugin process with socket path as argument
    let process = tokio::process::Command::new(&plugin_path)
      .arg(&socket_path) // Pass socket path directly as first argument
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .arg(socket_path.clone())
      .kill_on_drop(true)
      .spawn()?;

    // Convert socket path to proper format for connection
    let socket_name = socket_path
      .clone()
      .to_fs_name::<GenericFilePath>()
      .map_err(|e| PluginManagerError::SocketPath(e.to_string()))?;

    // Try to connect with timeout
    let client = timeout(Duration::from_secs(5), async {
      loop {
        match PluginClient::connect(&socket_name).await {
          Ok(client) => return Ok::<_, PluginClientError>(client),
          Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
      }
    })
    .await
    .map_err(|_| PluginManagerError::ConnectionError("Connection timeout".to_string()))?
    .map_err(|e| PluginManagerError::ConnectionError(e.to_string()))?;

    let client = Arc::new(Mutex::new(client));

    active_plugins.insert(
      plugin_name.to_string(),
      PluginInstance {
        process,
        socket_path,
        client,
      },
    );

    Ok(())
  }

  /// Get a reference to a plugin client
  pub async fn get_client(&self, plugin_name: &str) -> Result<Arc<Mutex<PluginClient>>> {
    let active_plugins = self.active_plugins.lock().await;

    if let Some(instance) = active_plugins.get(plugin_name) {
      Ok(Arc::clone(&instance.client))
    } else {
      Err(PluginManagerError::PluginNotFound(plugin_name.to_string()))
    }
  }

  /// Shutdown a specific plugin
  pub async fn shutdown_plugin(&self, plugin_name: &str) -> Result<()> {
    let mut active_plugins = self.active_plugins.lock().await;

    if let Some(mut instance) = active_plugins.remove(plugin_name) {
      // Get mutable access to client
      let mut client = instance.client.lock().await;
      client
        .shutdown()
        .await
        .map_err(|e| PluginManagerError::ConnectionError(e.to_string()))?;

      let _ = instance.process.kill().await;
      let _ = tokio::fs::remove_file(&instance.socket_path).await;

      Ok(())
    } else {
      Err(PluginManagerError::PluginNotFound(plugin_name.to_string()))
    }
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
}

impl Drop for PluginManager {
  fn drop(&mut self) {
    let active_plugins = Arc::clone(&self.active_plugins);
    tokio::spawn(async move {
      let mut plugins = active_plugins.lock().await;
      for (_, mut instance) in plugins.drain() {
        let _ = instance.process.kill().await;
        let _ = tokio::fs::remove_file(&instance.socket_path).await;
      }
    });
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::TempDir;

  async fn setup_test_environment() -> (PluginManager, TempDir, TempDir) {
    let plugins_dir = TempDir::new().unwrap();
    let socket_dir = TempDir::new().unwrap();

    // Create a dummy plugin executable
    #[cfg(unix)]
    {
      let plugin_path = plugins_dir.path().join("test_plugin");
      fs::write(
        &plugin_path,
        r#"#!/bin/sh
SOCKET_PATH="$1"
echo "Socket path: $SOCKET_PATH"
while true; do sleep 1; done
"#,
      )
      .unwrap();
      fs::set_permissions(&plugin_path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }

    #[cfg(windows)]
    {
      let plugin_path = plugins_dir.path().join("test_plugin.exe");
      // Create a dummy executable for Windows
      // This is just for testing - you'd need a real executable in practice
      fs::write(&plugin_path, "dummy content").unwrap();
    }

    let plugin_manager = PluginManager::new(plugins_dir.path());

    (plugin_manager, plugins_dir, socket_dir)
  }

  #[tokio::test]
  async fn test_plugin_lifecycle() {
    let (plugin_manager, _plugins_dir, _socket_dir) = setup_test_environment().await;

    // Test starting a plugin
    assert!(plugin_manager.start_plugin("test_plugin").await.is_ok());
    assert!(plugin_manager.is_plugin_running("test_plugin").await);

    // Test getting socket path
    let socket_path = plugin_manager.get_socket_path("test_plugin").await;
    assert!(socket_path.is_some());
    // assert!(socket_path.unwrap().contains("test_plugin"));

    // Test listing active plugins
    let active_plugins = plugin_manager.list_active_plugins().await;
    assert_eq!(active_plugins.len(), 1);
    assert_eq!(active_plugins[0], "test_plugin");

    // Test shutting down a plugin
    assert!(plugin_manager.shutdown_plugin("test_plugin").await.is_ok());
    assert!(!plugin_manager.is_plugin_running("test_plugin").await);
  }

  #[tokio::test]
  async fn test_plugin_error_handling() {
    let (plugin_manager, _plugins_dir, _socket_dir) = setup_test_environment().await;

    // Test starting non-existent plugin
    assert!(matches!(
      plugin_manager.start_plugin("nonexistent_plugin").await,
      Err(PluginManagerError::PluginNotFound(_))
    ));

    // Test double-starting a plugin
    assert!(plugin_manager.start_plugin("test_plugin").await.is_ok());
    assert!(matches!(
      plugin_manager.start_plugin("test_plugin").await,
      Err(PluginManagerError::PluginAlreadyRunning(_))
    ));
  }
}
