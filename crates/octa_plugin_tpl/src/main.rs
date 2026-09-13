//! Built-in text/template plugin.
//!
//! Inline templates have no filesystem inputs. File-backed templates expose
//! their exact source file through protocol-v2 cache planning, then retain the
//! existing execution behavior for cached and uncached tasks alike.

use std::{
  borrow::Cow,
  env,
  path::{Component, Path, PathBuf},
  sync::Arc,
};

use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use octa_plugin::{
  logger::Logger,
  protocol::{PluginCachePlan, PluginCachePlanRequest, PluginResponse},
  serve_plugin, Plugin, PluginCommand, PluginSchema,
};
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
    supports_raw: false,
    capabilities: Vec::new(),
    input_schema: serde_json::json!({
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
    output_schema: None,
  }
}

async fn load_template(value: Value, dir: &Path) -> anyhow::Result<String> {
  let Value::Object(params) = value else {
    return value
      .as_str()
      .map(str::to_owned)
      .context("template plugin value must be a string or file object");
  };

  let template_file = serde_json::from_value::<TemplateFile>(Value::Object(params))
    .context("Failed to parse template file parameters")?;
  let path = dir.join(template_file.file);

  tokio::fs::read_to_string(&path)
    .await
    .with_context(|| format!("Failed to read template file '{}'", path.display()))
}

/// Converts a plugin parameter with the same component semantics as execution.
///
/// Windows separators are recognized by `Path::components` on Windows. On
/// Unix a literal backslash remains inside a component and is rejected, so a
/// cache contract never aliases it to a different `/` path.
fn portable_relative_path(path: &Path) -> Option<String> {
  let mut result = Vec::new();
  for component in path.components() {
    match component {
      Component::Normal(value) => {
        let value = value.to_str()?;
        if value.contains(['\\', ':']) || value.chars().any(char::is_control) {
          return None;
        }
        result.push(value);
      },
      Component::CurDir | Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
    }
  }
  (!result.is_empty()).then(|| result.join("/"))
}

#[async_trait]
impl Plugin for TemplatePlugin {
  /// Return plugin version
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  fn cache_plan(&self, request: &PluginCachePlanRequest) -> anyhow::Result<Option<PluginCachePlan>> {
    let Value::Object(params) = &request.params else {
      // An inline template reads no workspace files. Its variables and
      // environment are already included by the action descriptor.
      return Ok(Some(PluginCachePlan::default()));
    };
    let template = serde_json::from_value::<TemplateFile>(Value::Object(params.clone()))
      .context("Failed to parse template file parameters")?;
    let Some(file) = portable_relative_path(&template.file) else {
      // Execution retains native Path behavior for uncached tasks. Cache
      // planning must fail closed when that path has no portable identity.
      return Ok(None);
    };
    let portable = if request.working_directory.is_empty() {
      file
    } else {
      format!("{}/{file}", request.working_directory)
    };
    let mut pattern = glob::Pattern::escape(&portable);
    if pattern.starts_with('!') {
      pattern.insert(0, '\\');
    }
    Ok(Some(PluginCachePlan {
      inputs: vec![pattern],
      outputs: Vec::new(),
    }))
  }

  async fn execute_command(
    &self,
    request: PluginCommand,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    _cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    let PluginCommand {
      id,
      value,
      dir,
      vars,
      envs,
      ..
    } = request;
    logger.log("Start processing template command")?;

    let mut tera = Tera::default();
    let template_name = format!("template_{}", id);
    let template = load_template(value, &dir).await?;

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

    let completed_response = PluginResponse::Completed {
      id: id.clone(),
      code: 0,
      outputs: Default::default(),
    };
    let completed_response_json = serde_json::to_string(&completed_response).unwrap() + "\n";

    let mut lock = writer.lock().await;
    let _ = lock.write_all(stdout_response_json.as_bytes()).await;
    let _ = logger.log(&stdout_response_json.to_string());

    let _ = lock.write_all(completed_response_json.as_bytes()).await;
    let _ = logger.log(&completed_response_json.to_string());

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
  use std::{collections::HashMap, io, time::Duration};
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
    assert_eq!(schema.input_schema.unwrap()["oneOf"].as_array().unwrap().len(), 2);
  }

  #[test]
  fn plans_inline_and_file_template_inputs_without_accessing_the_workspace() {
    let plugin = TemplatePlugin {};
    let target = octa_plugin::protocol::TargetPlatform {
      os: "linux".to_owned(),
      architecture: "x86_64".to_owned(),
    };
    let inline = plugin
      .cache_plan(&PluginCachePlanRequest {
        params: Value::String("Hello {{ name }}".to_owned()),
        working_directory: String::new(),
        target: target.clone(),
      })
      .unwrap()
      .unwrap();
    assert_eq!(inline, PluginCachePlan::default());

    // A string remains an inline template even when its contents happen to be
    // valid JSON. Planning must use the schema-selected value type rather than
    // reparsing the legacy execution string.
    let json_shaped_inline = plugin
      .cache_plan(&PluginCachePlanRequest {
        params: Value::String(r#"{"file":"not-an-input"}"#.to_owned()),
        working_directory: String::new(),
        target: target.clone(),
      })
      .unwrap()
      .unwrap();
    assert_eq!(json_shaped_inline, PluginCachePlan::default());

    let file = plugin
      .cache_plan(&PluginCachePlanRequest {
        params: serde_json::json!({ "file": "templates/[name].txt" }),
        working_directory: "project".to_owned(),
        target: target.clone(),
      })
      .unwrap()
      .unwrap();
    assert_eq!(file.inputs, ["project/templates/[[]name[]].txt"]);
    assert!(file.outputs.is_empty());

    let leading_bang = plugin
      .cache_plan(&PluginCachePlanRequest {
        params: serde_json::json!({ "file": "!template.txt" }),
        working_directory: String::new(),
        target,
      })
      .unwrap()
      .unwrap();
    assert_eq!(leading_bang.inputs, [r"\!template.txt"]);

    for file in ["../outside", "/absolute", "bad\nname"] {
      let plan = plugin
        .cache_plan(&PluginCachePlanRequest {
          params: serde_json::json!({ "file": file }),
          working_directory: String::new(),
          target: octa_plugin::protocol::TargetPlatform {
            os: "linux".to_owned(),
            architecture: "x86_64".to_owned(),
          },
        })
        .unwrap();
      assert!(plan.is_none(), "{file}");
    }

    #[cfg(unix)]
    assert!(plugin
      .cache_plan(&PluginCachePlanRequest {
        params: serde_json::json!({ "file": r"a\b" }),
        working_directory: String::new(),
        target: octa_plugin::protocol::TargetPlatform {
          os: "linux".to_owned(),
          architecture: "x86_64".to_owned(),
        },
      })
      .unwrap()
      .is_none());
  }

  #[tokio::test]
  async fn json_shaped_string_remains_an_inline_template_during_execution() {
    let directory = tempfile::tempdir().unwrap();
    let inline = r#"{"file":"not-a-file"}"#;

    assert_eq!(
      load_template(Value::String(inline.to_owned()), directory.path())
        .await
        .unwrap(),
      inline
    );
  }

  #[tokio::test]
  async fn test_echo_command() {
    let (writer, logger, dir) = setup_test().await;
    let plugin = TemplatePlugin {};
    let test_string = "Hello, World!";
    let cancel_token = CancellationToken::new();

    let result = plugin
      .execute_command(
        PluginCommand {
          id: "test-id".to_string(),
          dry: false,
          value: Value::String("{{ name }}".to_owned()),
          args: vec![],
          dir,
          vars: HashMap::from([("name".to_owned(), Value::String("Hello, World!".to_owned()))]),
          envs: HashMap::new(),
          raw: false,
          input: tokio::sync::mpsc::unbounded_channel().1,
        },
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
        PluginCommand {
          id: "test-id".to_string(),
          dry: false,
          value: serde_json::json!({ "file": "greeting.tpl" }),
          args: vec![],
          dir,
          vars: HashMap::from([("name".to_owned(), Value::String("World".to_owned()))]),
          envs: HashMap::new(),
          raw: false,
          input: tokio::sync::mpsc::unbounded_channel().1,
        },
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
        PluginCommand {
          id: "test-id".to_string(),
          dry: false,
          value: serde_json::json!({ "file": "missing.tpl" }),
          args: vec![],
          dir,
          vars: HashMap::new(),
          envs: HashMap::new(),
          raw: false,
          input: tokio::sync::mpsc::unbounded_channel().1,
        },
        writer,
        logger,
        CancellationToken::new(),
      )
      .await
      .unwrap_err();

    assert!(error.to_string().contains("Failed to read template file"));
  }
}
