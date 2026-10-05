use std::{
  any::Any,
  collections::HashMap,
  fs::{File, OpenOptions},
  io::{self, Write},
  path::PathBuf,
  sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
  },
  thread,
};

use async_trait::async_trait;

use chrono::Local;
/// Stable marker inserted wherever a resolved secret value was removed.
pub const REDACTION_MARKER: &str = "*****";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RedactionMatch {
  Substring,
  Token,
}

/// One resolved secret value together with its safe free-text matching rule.
///
/// String secrets are replaced wherever they occur. JSON numbers and booleans
/// are replaced only as standalone tokens so a value such as `42` does not
/// corrupt an unrelated version (`v42`) or path component.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Redaction {
  value: String,
  match_kind: RedactionMatch,
}

impl Redaction {
  /// Returns the exact UTF-8 representation used by conservative byte-stream redaction.
  ///
  /// A byte stream has no reliable token boundaries across arbitrary chunks,
  /// so consumers should replace this complete sequence wherever it occurs.
  pub fn as_bytes(&self) -> &[u8] {
    self.value.as_bytes()
  }
}

#[async_trait]
pub trait Logger: Send + Sync + Any + 'static {
  fn log(&self, message: &str) -> anyhow::Result<()>;

  fn as_any(&self) -> &dyn Any;
}

/// Replaces resolved secret values before a plugin writes diagnostic messages.
pub fn redact(message: &str, secrets: &[Redaction]) -> String {
  let mut secrets: Vec<&Redaction> = secrets.iter().filter(|secret| !secret.value.is_empty()).collect();
  // Replace longer values first so an overlapping prefix cannot leave a secret suffix behind.
  secrets.sort_unstable_by_key(|secret| {
    (
      std::cmp::Reverse(secret.value.len()),
      matches!(secret.match_kind, RedactionMatch::Token),
    )
  });
  secrets
    .into_iter()
    .fold(message.to_owned(), |message, secret| match secret.match_kind {
      RedactionMatch::Substring => message.replace(&secret.value, REDACTION_MARKER),
      RedactionMatch::Token => replace_token(&message, &secret.value),
    })
}

/// Collects scalar leaves because templates can expose individual fields of a structured secret.
///
/// The scalar's JSON type is retained so free-text redaction can distinguish a
/// string secret from a low-entropy numeric or boolean token.
pub fn collect_value_redactions(value: &serde_json::Value, redactions: &mut Vec<Redaction>) {
  match value {
    serde_json::Value::String(value) if !value.is_empty() => redactions.push(Redaction {
      value: value.clone(),
      match_kind: RedactionMatch::Substring,
    }),
    serde_json::Value::String(_) | serde_json::Value::Null => {},
    serde_json::Value::Bool(_) | serde_json::Value::Number(_) => redactions.push(Redaction {
      value: value.to_string(),
      match_kind: RedactionMatch::Token,
    }),
    serde_json::Value::Array(values) => {
      for value in values {
        collect_value_redactions(value, redactions);
      }
    },
    serde_json::Value::Object(values) => {
      for value in values.values() {
        collect_value_redactions(value, redactions);
      }
    },
  }
}

/// Resolves scalar redaction values only from variables marked as secret.
///
/// Unknown names are ignored because a variable can be absent after platform
/// or task filtering. Ordinary variables are deliberately excluded so their
/// diagnostic values remain useful to plugin authors.
pub fn collect_variable_redactions(
  vars: &HashMap<String, serde_json::Value>,
  secret_vars: &[String],
) -> Vec<Redaction> {
  let mut redactions = Vec::new();
  for value in secret_vars.iter().filter_map(|name| vars.get(name)) {
    collect_value_redactions(value, &mut redactions);
  }
  redactions
}

fn replace_token(message: &str, secret: &str) -> String {
  let mut result = String::with_capacity(message.len());
  let mut copied_until = 0;
  for (start, _) in message.match_indices(secret) {
    let end = start + secret.len();
    let begins_at_boundary = message[..start]
      .chars()
      .next_back()
      .is_none_or(|character| !is_token_character(character));
    let ends_at_boundary = message[end..]
      .chars()
      .next()
      .is_none_or(|character| !is_token_character(character));
    if begins_at_boundary && ends_at_boundary {
      result.push_str(&message[copied_until..start]);
      result.push_str(REDACTION_MARKER);
      copied_until = end;
    }
  }
  result.push_str(&message[copied_until..]);
  result
}

fn is_token_character(character: char) -> bool {
  character.is_alphanumeric() || character == '_'
}

/// Logger adapter that removes resolved secret values before delegating a message.
pub struct RedactingLogger<L> {
  logger: Arc<L>,
  secrets: Vec<Redaction>,
}

impl<L> RedactingLogger<L> {
  /// Wraps a logger with the scalar secret values that must be replaced.
  pub fn new(logger: Arc<L>, secrets: Vec<Redaction>) -> Self {
    Self { logger, secrets }
  }
}

impl<L: Logger> Logger for RedactingLogger<L> {
  fn log(&self, message: &str) -> anyhow::Result<()> {
    self.logger.log(&redact(message, &self.secrets))
  }

  fn as_any(&self) -> &dyn Any {
    self
  }
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
  writer_handle: Option<thread::JoinHandle<()>>,
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

    // Silent logging never queues messages, so it does not need a thread
    // waiting on an otherwise idle channel for the whole plugin lifetime.
    let writer_handle = if logger.silent {
      None
    } else {
      Some(
        thread::Builder::new()
          .name(format!("octa-plugin-{plugin_name}-logger"))
          .spawn(move || log_writer.run())?,
      )
    };

    Ok(Self { logger, writer_handle })
  }

  pub fn get_logger(&self) -> Arc<PluginLogger> {
    Arc::clone(&self.logger)
  }

  pub fn shutdown(self) -> anyhow::Result<()> {
    // Reject new messages before the sentinel is queued so none can be
    // accepted behind it and silently left unwritten.
    self.logger.is_shutdown.store(true, Ordering::SeqCst);
    let _ = self.logger.send_shutdown();

    drop(self.logger);

    if let Some(writer_handle) = self.writer_handle {
      writer_handle
        .join()
        .map_err(|e| io::Error::other(format!("Failed to join logger thread: {:?}", e)))?;
    }

    Ok(())
  }
}

#[derive(Clone)]
pub struct MockLogger {
  messages: Arc<Mutex<Vec<String>>>,
}

impl MockLogger {
  pub fn new() -> Self {
    Self {
      messages: Arc::new(Mutex::new(Vec::new())),
    }
  }

  pub fn get_messages(&self) -> Vec<String> {
    self.messages.lock().expect("mock logger lock poisoned").clone()
  }
}

impl Default for MockLogger {
  fn default() -> Self {
    Self::new()
  }
}

impl Logger for MockLogger {
  fn log(&self, message: &str) -> anyhow::Result<()> {
    self
      .messages
      .lock()
      .map_err(|_| anyhow::anyhow!("mock logger lock poisoned"))?
      .push(message.to_owned());
    Ok(())
  }

  fn as_any(&self) -> &dyn Any {
    self
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs;
  use tempfile::tempdir;

  #[test]
  fn redacts_longest_secret_values_first() {
    let mut redactions = Vec::new();
    collect_value_redactions(&serde_json::json!(["token", "token-123", ""]), &mut redactions);
    let message = redact("token-123 token", &redactions);

    assert_eq!(message, "***** *****");
  }

  #[test]
  fn collects_redactions_from_structured_values() {
    let mut redactions = Vec::new();

    collect_value_redactions(
      &serde_json::json!({ "token": "private", "nested": [42, true, null] }),
      &mut redactions,
    );
    redactions.sort_by(|left, right| left.value.cmp(&right.value));

    assert_eq!(
      redactions
        .iter()
        .map(|redaction| redaction.value.as_str())
        .collect::<Vec<_>>(),
      ["42", "private", "true"]
    );
  }

  #[test]
  fn typed_scalars_are_redacted_only_as_standalone_tokens() {
    let mut redactions = Vec::new();
    collect_value_redactions(&serde_json::json!([42, false]), &mut redactions);

    assert_eq!(
      redact("v42 falsehood; values: 42 false", &redactions),
      "v42 falsehood; values: ***** *****"
    );
  }

  #[test]
  fn resolves_nested_secret_variables_without_collecting_ordinary_values() {
    let vars = HashMap::from([
      (
        "SECRET".to_owned(),
        serde_json::json!({ "token": "private", "nested": [42, true, null] }),
      ),
      ("PUBLIC".to_owned(), serde_json::json!({ "value": "visible" })),
    ]);

    let mut redactions = collect_variable_redactions(&vars, &["SECRET".to_owned(), "MISSING".to_owned()]);
    redactions.sort_by(|left, right| left.value.cmp(&right.value));

    assert_eq!(
      redactions
        .iter()
        .map(|redaction| redaction.value.as_str())
        .collect::<Vec<_>>(),
      ["42", "private", "true"]
    );
    assert!(!redactions.iter().any(|redaction| redaction.value == "visible"));
  }

  #[test]
  fn redacting_logger_masks_messages() {
    let logger = Arc::new(MockLogger::new());
    let mut redactions = Vec::new();
    collect_value_redactions(&serde_json::json!("private-value"), &mut redactions);
    let redacting = RedactingLogger::new(logger.clone(), redactions);

    redacting.log("using private-value").unwrap();

    assert_eq!(logger.get_messages(), vec!["using *****"]);
  }

  #[test]
  fn test_logger_with_file() -> io::Result<()> {
    let temp_dir = tempdir()?;
    let log_dir = temp_dir.path().to_string_lossy().to_string();

    let logger_system = LoggerSystem::new("test_plugin", Some(log_dir.clone()))?;
    let logger = logger_system.get_logger();

    // Test logging multiple messages
    logger.log("Test message 1").expect("Can't write log message");
    logger.log("Test message 2").expect("Can't write log message");

    // Shutdown waits for every queued message to be written and flushed.
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

    // Shutdown waits for every queued message to be written and flushed.
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

    // Shutdown waits for the queued message to be written and flushed.
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
