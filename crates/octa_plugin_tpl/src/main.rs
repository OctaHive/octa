use std::{
  borrow::Cow,
  collections::HashMap,
  env,
  path::{Path, PathBuf},
  sync::Arc,
};

use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use octa_plugin::{logger::Logger, protocol::PluginResponse, serve_plugin, Plugin, PluginSchema};
use tera::{Context as TeraContext, Tera};
use tokio::{
  io::{AsyncWrite, AsyncWriteExt},
  sync::Mutex,
};
use tokio_util::sync::CancellationToken;

struct TemplatePlugin {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TemplateFile {
  file: PathBuf,
}

fn plugin_schema() -> PluginSchema {
  PluginSchema {
    key: "tpl".to_owned(),
    capabilities: Vec::new(),
    validation_schema: serde_json::json!({
      "oneOf": [
        { "type": "string" },
        {
          "type": "object",
          "properties": {
            "file": { "type": "string", "minLength": 1 }
          },
          "required": ["file"],
          "additionalProperties": false
        }
      ]
    })
    .as_object()
    .cloned(),
  }
}

async fn load_template(command: String, dir: &Path) -> anyhow::Result<String> {
  let Ok(Value::Object(params)) = serde_json::from_str::<Value>(&command) else {
    return Ok(command);
  };

  let template_file = serde_json::from_value::<TemplateFile>(Value::Object(params))
    .context("Failed to parse template file parameters")?;
  let path = dir.join(template_file.file);

  tokio::fs::read_to_string(&path)
    .await
    .with_context(|| format!("Failed to read template file '{}'", path.display()))
}

#[async_trait]
impl Plugin for TemplatePlugin {
  /// Return plugin version
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  async fn execute_command(
    &self,
    id: String,
    _dry: bool,
    command: String,
    _args: Vec<String>,
    dir: PathBuf,
    vars: HashMap<String, Value>,
    envs: HashMap<String, String>,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    _cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    logger.log("Start processing template command")?;

    let mut tera = Tera::default();
    let template_name = format!("template_{}", id);
    let template = load_template(command, &dir).await?;

    let get_env = |name: &str| match envs.get(name) {
      Some(val) => Some(Cow::Borrowed(val.as_str())),
      None => match env::var(name) {
        Ok(val) => Some(Cow::Owned(val)),
        Err(_) => None,
      },
    };

    let val = shellexpand::env_with_context_no_errors(&template, get_env);

    tera
      .add_raw_template(&template_name, val.as_ref())
      .context("Failed to parse template")?;

    let context = TeraContext::from_serialize(&vars).context("Failed to serialize variables to context")?;

    let result = tera
      .render(&template_name, &context)
      .context(format!("Failed to render template: {:?}", context))?;

    let stdout_response = PluginResponse::Stdout {
      id: id.clone(),
      line: result,
    };
    let stdout_response_json = serde_json::to_string(&stdout_response).unwrap() + "\n";

    let exit_response = PluginResponse::ExitStatus {
      id: id.clone(),
      code: 0,
    };
    let exit_response_json = serde_json::to_string(&exit_response).unwrap() + "\n";

    let mut lock = writer.lock().await;
    let _ = lock.write_all(stdout_response_json.as_bytes()).await;
    let _ = logger.log(&stdout_response_json.to_string());

    let _ = lock.write_all(exit_response_json.as_bytes()).await;
    let _ = logger.log(&exit_response_json.to_string());

    let _ = lock.flush().await;

    Ok(())
  }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  serve_plugin(TemplatePlugin {}, plugin_schema()).await
}

#[cfg(test)]
mod tests {
  use super::*;
  use octa_plugin::logger::{Logger, MockLogger};
  use std::{io, time::Duration};
  use tempfile::tempdir;
  use tokio::sync::Mutex;

  struct TestWriter {
    buffer: Vec<u8>,
  }

  impl TestWriter {
    fn new() -> Self {
      Self { buffer: Vec::new() }
    }

    fn get_output(&self) -> String {
      String::from_utf8_lossy(&self.buffer).to_string()
    }
  }

  impl AsyncWrite for TestWriter {
    fn poll_write(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
      buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
      let this = self.get_mut();
      // Use explicit Write trait implementation
      std::io::Write::write_all(&mut this.buffer, buf).map_err(std::io::Error::other)?;
      std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
      self: std::pin::Pin<&mut Self>,
      _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
      std::task::Poll::Ready(Ok(()))
    }
  }

  async fn setup_test() -> (Arc<Mutex<TestWriter>>, Arc<impl Logger>, PathBuf) {
    let writer = Arc::new(Mutex::new(TestWriter::new()));
    let logger = Arc::new(MockLogger::new());
    let temp_dir = tempdir().unwrap();
    (writer, logger, temp_dir.keep())
  }

  #[tokio::test]
  async fn test_template_plugin_version() {
    let plugin = TemplatePlugin {};
    assert_eq!(plugin.version(), env!("CARGO_PKG_VERSION").to_string());
  }

  #[test]
  fn test_template_plugin_schema() {
    let schema = plugin_schema();

    assert_eq!(schema.key, "tpl");
    assert!(schema.capabilities.is_empty());
    assert_eq!(schema.validation_schema.unwrap()["oneOf"].as_array().unwrap().len(), 2);
  }

  #[tokio::test]
  async fn test_echo_command() {
    let (writer, logger, dir) = setup_test().await;
    let plugin = TemplatePlugin {};
    let test_string = "Hello, World!";
    let cancel_token = CancellationToken::new();

    let result = plugin
      .execute_command(
        "test-id".to_string(),
        false,
        "{{ name }}".to_owned(),
        vec![],
        dir,
        HashMap::from([("name".to_owned(), Value::String("Hello, World!".to_owned()))]),
        HashMap::new(),
        writer.clone(),
        logger.clone(),
        cancel_token,
      )
      .await;

    assert!(result.is_ok());

    // Wait a bit for async logging to complete
    tokio::time::sleep(Duration::from_millis(100)).await;

    let output = writer.lock().await.get_output();
    let lines: Vec<&str> = output.lines().collect();

    // Find stdout message
    let stdout_line = lines
      .iter()
      .find(|line| line.contains("\"type\":\"Stdout\""))
      .expect("Should have stdout message");

    let response: PluginResponse = serde_json::from_str(stdout_line).unwrap();
    match response {
      PluginResponse::Stdout { id, line } => {
        assert_eq!(id, "test-id");
        assert!(line.contains(test_string));
      },
      _ => panic!("Expected Stdout response"),
    }

    // Check logger messages using as_any()
    let mock_logger = logger.as_any().downcast_ref::<MockLogger>().unwrap();
    let log_messages = mock_logger.get_messages().await;
    assert!(!log_messages.is_empty());
    assert!(log_messages.iter().any(|msg| msg.contains("Stdout")));
  }

  #[tokio::test]
  async fn test_file_template() {
    let (writer, logger, dir) = setup_test().await;
    tokio::fs::write(dir.join("greeting.tpl"), "Hello, {{ name }}!")
      .await
      .unwrap();

    TemplatePlugin {}
      .execute_command(
        "test-id".to_string(),
        false,
        serde_json::json!({ "file": "greeting.tpl" }).to_string(),
        vec![],
        dir,
        HashMap::from([("name".to_owned(), Value::String("World".to_owned()))]),
        HashMap::new(),
        writer.clone(),
        logger,
        CancellationToken::new(),
      )
      .await
      .unwrap();

    let output = writer.lock().await.get_output();
    let stdout = output
      .lines()
      .find(|line| line.contains("\"type\":\"Stdout\""))
      .unwrap();
    let response: PluginResponse = serde_json::from_str(stdout).unwrap();

    assert!(matches!(
      response,
      PluginResponse::Stdout { line, .. } if line == "Hello, World!"
    ));
  }

  #[tokio::test]
  async fn test_missing_template_file() {
    let (writer, logger, dir) = setup_test().await;

    let error = TemplatePlugin {}
      .execute_command(
        "test-id".to_string(),
        false,
        serde_json::json!({ "file": "missing.tpl" }).to_string(),
        vec![],
        dir,
        HashMap::new(),
        HashMap::new(),
        writer,
        logger,
        CancellationToken::new(),
      )
      .await
      .unwrap_err();

    assert!(error.to_string().contains("Failed to read template file"));
  }
}
