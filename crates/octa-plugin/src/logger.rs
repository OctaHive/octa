use std::{
  any::Any,
  fs::{File, OpenOptions},
  io::{self, Write},
  path::PathBuf,
  sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc,
  },
  thread,
};

use async_trait::async_trait;

use chrono::Local;
use tokio::sync::Mutex;

#[async_trait]
pub trait Logger: Send + Sync + Any + 'static {
  fn log(&self, message: &str) -> anyhow::Result<()>;

  fn as_any(&self) -> &dyn Any;
}

pub enum LogMessage {
  Normal { timestamp: String, message: String },
  Shutdown,
}

pub struct PluginLogger {
  tx: mpsc::SyncSender<LogMessage>,
  is_shutdown: Arc<AtomicBool>,
  silent: bool,
}

pub struct LogWriter {
  rx: mpsc::Receiver<LogMessage>,
  log_file: Option<File>,
}

pub struct LoggerSystem {
  logger: Arc<PluginLogger>,
  writer_handle: thread::JoinHandle<()>,
}

impl PluginLogger {
  pub fn new(plugin_name: &str, log_dir: Option<String>) -> io::Result<(Self, LogWriter)> {
    let log_file = if log_dir.is_some() {
      let logs_dir = log_dir.clone().unwrap();
      std::fs::create_dir_all(&logs_dir)?;

      let log_path = PathBuf::from(logs_dir).join(format!("{}.log", plugin_name));
      Some(OpenOptions::new().create(true).append(true).open(log_path)?)
    } else {
      None
    };

    let (tx, rx) = mpsc::sync_channel(100);

    Ok((
      Self {
        tx,
        is_shutdown: Arc::new(AtomicBool::new(false)),
        silent: log_dir.is_none(),
      },
      LogWriter { rx, log_file },
    ))
  }

  pub fn log(&self, message: &str) -> Result<(), mpsc::SendError<LogMessage>> {
    if self.is_shutdown.load(Ordering::SeqCst) || self.silent {
      return Ok(());
    }

    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string();
    self.tx.send(LogMessage::Normal {
      timestamp,
      message: message.to_string(),
    })
  }

  fn send_shutdown(&self) -> Result<(), mpsc::SendError<LogMessage>> {
    self.tx.send(LogMessage::Shutdown)
  }
}

impl Logger for PluginLogger {
  fn log(&self, message: &str) -> anyhow::Result<()> {
    // Your existing log implementation
    self.log(message).map_err(|e| anyhow::anyhow!(e))
  }

  fn as_any(&self) -> &dyn Any {
    self
  }
}

impl LogWriter {
  pub fn run(&mut self) {
    while let Ok(log_msg) = self.rx.recv() {
      match log_msg {
        LogMessage::Normal { timestamp, message } => {
          if let Some(file) = &mut self.log_file {
            let log_line = format!("[{}] {}\n", timestamp, message);

            if let Err(e) = file.write_all(log_line.as_bytes()) {
              eprintln!("Failed to write to log file: {}", e);
            }
            if let Err(e) = file.flush() {
              eprintln!("Failed to flush log file: {}", e);
            }
          }
        },
        LogMessage::Shutdown => break,
      }
    }
  }
}

impl LoggerSystem {
  pub fn new(plugin_name: &str, log_dir: Option<String>) -> io::Result<Self> {
    let (logger, mut log_writer) = PluginLogger::new(plugin_name, log_dir)?;
    let logger = Arc::new(logger);

    // Spawn the log writer in a separate thread
    let writer_handle = thread::spawn(move || {
      log_writer.run();
    });

    Ok(Self { logger, writer_handle })
  }

  pub fn get_logger(&self) -> Arc<PluginLogger> {
    Arc::clone(&self.logger)
  }

  pub fn shutdown(self) -> anyhow::Result<()> {
    // Send shutdown message before setting the flag
    let _ = self.logger.send_shutdown();

    // Now set the shutdown flag
    self.logger.is_shutdown.store(true, Ordering::SeqCst);

    // Drop the sender
    drop(self.logger);

    // Wait for the writer thread to complete
    self
      .writer_handle
      .join()
      .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("Failed to join logger thread: {:?}", e)))?;

    Ok(())
  }
}

// Add MockLogger implementation
#[derive(Clone)]
pub struct MockLogger {
  messages: Arc<Mutex<Vec<String>>>,
  log_count: Arc<AtomicUsize>,
}

impl MockLogger {
  pub fn new() -> Self {
    Self {
      messages: Arc::new(Mutex::new(Vec::new())),
      log_count: Arc::new(AtomicUsize::new(0)),
    }
  }

  pub async fn get_messages(&self) -> Vec<String> {
    self.messages.lock().await.clone()
  }
}

impl Default for MockLogger {
  fn default() -> Self {
    Self::new()
  }
}

impl Logger for MockLogger {
  fn log(&self, message: &str) -> anyhow::Result<()> {
    self.log_count.fetch_add(1, Ordering::SeqCst);
    let messages = self.messages.clone();
    let message = message.to_string();
    tokio::spawn(async move {
      messages.lock().await.push(message);
    });
    Ok(())
  }

  fn as_any(&self) -> &dyn Any {
    self
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::{fs, time::Duration};
  use tempfile::tempdir;

  #[test]
  fn test_logger_with_file() -> io::Result<()> {
    let temp_dir = tempdir()?;
    let log_dir = temp_dir.path().to_string_lossy().to_string();

    let logger_system = LoggerSystem::new("test_plugin", Some(log_dir.clone()))?;
    let logger = logger_system.get_logger();

    // Test logging multiple messages
    logger.log("Test message 1").expect("Can't write log message");
    logger.log("Test message 2").expect("Can't write log message");

    // Allow time for messages to be written
    std::thread::sleep(Duration::from_millis(100));

    // Shutdown logger
    logger_system.shutdown().expect("Can't shutdown logger system");

    // Read log file
    let log_path = PathBuf::from(&log_dir).join("test_plugin.log");
    let log_content = fs::read_to_string(log_path)?;

    // Verify log content
    assert!(log_content.contains("Test message 1"));
    assert!(log_content.contains("Test message 2"));
    assert!(log_content.contains("[20")); // Check timestamp format

    Ok(())
  }

  #[test]
  fn test_logger_silent_mode() -> io::Result<()> {
    let logger_system = LoggerSystem::new("test_plugin", None)?;
    let logger = logger_system.get_logger();

    // These logs should be silently ignored
    assert!(logger.log("Silent message 1").is_ok());
    assert!(logger.log("Silent message 2").is_ok());

    logger_system.shutdown().expect("Can't shutdown logger system");
    Ok(())
  }

  #[test]
  fn test_logger_shutdown_behavior() -> io::Result<()> {
    let temp_dir = tempdir()?;
    let log_dir = temp_dir.path().to_string_lossy().to_string();

    let logger_system = LoggerSystem::new("test_plugin", Some(log_dir.clone()))?;
    let logger = logger_system.get_logger();

    // Log before shutdown
    logger.log("Before shutdown").expect("Can't write log message");

    // Shutdown logger
    logger_system.shutdown().expect("Can't shutdown logger system");

    // Attempt to log after shutdown - should be ignored
    assert!(logger.log("After shutdown").is_ok());

    // Read log file
    let log_path = PathBuf::from(&log_dir).join("test_plugin.log");
    let log_content = fs::read_to_string(log_path)?;

    // Verify only pre-shutdown message exists
    assert!(log_content.contains("Before shutdown"));
    assert!(!log_content.contains("After shutdown"));

    Ok(())
  }

  #[test]
  fn test_logger_concurrent_writes() -> io::Result<()> {
    let temp_dir = tempdir()?;
    let log_dir = temp_dir.path().to_string_lossy().to_string();

    let logger_system = LoggerSystem::new("test_plugin", Some(log_dir.clone()))?;
    let logger = logger_system.get_logger();
    let logger_clone = logger_system.get_logger();

    // Spawn multiple threads to write logs concurrently
    let handle1 = std::thread::spawn({
      let logger = Arc::clone(&logger);
      move || {
        for i in 0..100 {
          logger.log(&format!("Thread 1 message {}", i)).unwrap();
        }
      }
    });

    let handle2 = std::thread::spawn({
      let logger = Arc::clone(&logger_clone);
      move || {
        for i in 0..100 {
          logger.log(&format!("Thread 2 message {}", i)).unwrap();
        }
      }
    });

    // Wait for threads to complete
    handle1.join().unwrap();
    handle2.join().unwrap();

    // Allow time for messages to be written
    std::thread::sleep(Duration::from_millis(100));

    // Shutdown logger
    logger_system.shutdown().expect("Can't shutdown logger system");

    // Read log file
    let log_path = PathBuf::from(&log_dir).join("test_plugin.log");
    let log_content = fs::read_to_string(log_path)?;

    // Verify all messages were written
    let thread1_count = log_content.matches("Thread 1 message").count();
    let thread2_count = log_content.matches("Thread 2 message").count();

    assert_eq!(thread1_count, 100);
    assert_eq!(thread2_count, 100);

    Ok(())
  }

  #[test]
  fn test_logger_message_format() -> io::Result<()> {
    let temp_dir = tempdir()?;
    let log_dir = temp_dir.path().to_string_lossy().to_string();

    let logger_system = LoggerSystem::new("test_plugin", Some(log_dir.clone()))?;
    let logger = logger_system.get_logger();

    // Log a message
    logger.log("Test message").expect("Can't write log message");

    // Allow time for message to be written
    std::thread::sleep(Duration::from_millis(100));

    // Shutdown logger
    logger_system.shutdown().expect("Can't shutdown logger system");

    // Read log file
    let log_path = PathBuf::from(&log_dir).join("test_plugin.log");
    let log_content = fs::read_to_string(log_path)?;

    // Check log format using regex
    let re = regex::Regex::new(r"^\[\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\.\d{3}\] Test message\n$").unwrap();
    assert!(re.is_match(&log_content));

    Ok(())
  }
}
