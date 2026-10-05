//! Official Codex task plugin.
//!
//! The plugin is a thin coordinator around five private modules: task
//! configuration, invocation construction, process ownership, event decoding,
//! and durable run records. Keeping those boundaries private lets the concrete
//! Codex CLI integration evolve without exposing a speculative backend API.

use std::sync::Arc;

use async_trait::async_trait;
use octa_plugin::{
  logger::Logger,
  protocol::{PluginCachePlan, PluginCachePlanRequest, PluginResponse},
  serve_plugin, Plugin, PluginCommand, PluginSchema,
};
use tokio::{
  io::{AsyncWrite, AsyncWriteExt},
  sync::Mutex,
};
use tokio_util::sync::CancellationToken;

mod config;
mod events;
mod invocation;
mod process;
mod records;

struct CodexPlugin;

fn plugin_schema() -> PluginSchema {
  PluginSchema {
    key: "codex".to_owned(),
    supports_raw: false,
    capabilities: Vec::new(),
    input_schema: Some(config::input_schema()),
    output_schema: Some(records::output_schema()),
  }
}

#[async_trait]
impl Plugin for CodexPlugin {
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  /// Keeps Codex runs opaque because external model and tool state cannot be
  /// represented by a complete automatic filesystem contract.
  fn cache_plan(&self, _request: &PluginCachePlanRequest) -> anyhow::Result<Option<PluginCachePlan>> {
    Ok(None)
  }

  async fn execute_command(
    &self,
    command: PluginCommand,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + Unpin>>,
    _logger: Arc<impl Logger>,
    _cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    let PluginCommand { id, dry, value, .. } = command;
    let _config = config::CodexConfig::parse(value)?;
    if dry {
      return send_completed(&writer, id, 0, records::dry_run_outputs()).await;
    }
    anyhow::bail!("Codex task contract is not implemented")
  }
}

async fn send_completed(
  writer: &Arc<Mutex<impl AsyncWrite + Send + 'static + Unpin>>,
  id: String,
  code: i32,
  outputs: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
  records::validate_outputs(&outputs)?;
  send_response(writer, &PluginResponse::Completed { id, code, outputs }).await
}

async fn send_response(
  writer: &Arc<Mutex<impl AsyncWrite + Send + 'static + Unpin>>,
  response: &PluginResponse,
) -> anyhow::Result<()> {
  let mut message = serde_json::to_vec(response)?;
  message.push(b'\n');
  let mut writer = writer.lock().await;
  writer.write_all(&message).await?;
  writer.flush().await?;
  Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  serve_plugin(CodexPlugin, plugin_schema()).await
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;
