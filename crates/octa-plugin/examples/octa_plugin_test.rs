//! Process fixture used by cross-platform plugin manager and executor tests.
//!
//! Keeping the fixture on the Rust SDK exercises the real local-socket
//! protocol without making Windows tests depend on Python and pywin32.

use std::sync::Arc;

use async_trait::async_trait;
use octa_plugin::{
  logger::Logger,
  protocol::{PluginCachePlan, PluginCachePlanRequest, PluginResponse},
  serve_plugin, Plugin, PluginCommand, PluginSchema,
};
use serde_json::{Map, Value};
use tokio::{
  io::{AsyncWrite, AsyncWriteExt},
  sync::Mutex,
};
use tokio_util::sync::CancellationToken;

struct TestPlugin;

fn schema() -> PluginSchema {
  PluginSchema {
    key: "key".to_owned(),
    supports_raw: false,
    capabilities: Vec::new(),
    input_schema: None,
    output_schema: serde_json::json!({
      "type": "object",
      "properties": {
        "digest": { "type": "string" }
      },
      "required": ["digest"],
      "additionalProperties": false
    })
    .as_object()
    .cloned(),
  }
}

#[async_trait]
impl Plugin for TestPlugin {
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  fn cache_plan(&self, request: &PluginCachePlanRequest) -> anyhow::Result<Option<PluginCachePlan>> {
    match request.params.as_str() {
      Some("plan-error") => anyhow::bail!("fixture planning failure"),
      Some("planned") => Ok(Some(PluginCachePlan {
        inputs: vec![format!("platform/{}.input", request.target.os)],
        outputs: Vec::new(),
      })),
      _ => Ok(None),
    }
  }

  async fn execute_command(
    &self,
    command: PluginCommand,
    writer: Arc<Mutex<impl AsyncWrite + Send + Unpin + 'static>>,
    _logger: Arc<impl Logger>,
    _cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    let stdout = PluginResponse::Stdout {
      id: command.id.clone(),
      line: "test output".to_owned(),
    };
    writer
      .lock()
      .await
      .write_all((serde_json::to_string(&stdout)? + "\n").as_bytes())
      .await?;

    let completed = PluginResponse::Completed {
      id: command.id,
      code: 0,
      outputs: Map::from_iter([("digest".to_owned(), Value::String("sha256:test".to_owned()))]),
    };
    writer
      .lock()
      .await
      .write_all((serde_json::to_string(&completed)? + "\n").as_bytes())
      .await?;
    Ok(())
  }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  serve_plugin(TestPlugin, schema()).await
}
