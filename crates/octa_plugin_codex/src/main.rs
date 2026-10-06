//! Official Codex task plugin.
//!
//! The plugin is organized around private modules for task configuration,
//! invocation construction, process ownership, event decoding, sanitization,
//! and durable run records. Keeping those boundaries private lets the concrete
//! Codex CLI integration evolve without exposing a speculative backend API.

use std::sync::Arc;

use async_trait::async_trait;
use octa_plugin::{
  logger::Logger,
  protocol::{PluginCachePlan, PluginCachePlanRequest, PluginResponse},
  send_response, serve_plugin, Plugin, PluginCommand, PluginSchema,
};
use tokio::{io::AsyncWrite, sync::Mutex};
use tokio_util::sync::CancellationToken;

mod command;
mod config;
mod contract;
mod events;
mod filesystem;
mod invocation;
mod process;
mod records;
mod resources;
mod sanitization;

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
    cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    let PluginCommand {
      id,
      dry,
      value,
      dir,
      vars,
      secret_vars,
      envs,
      ..
    } = command;
    let config = config::CodexConfig::parse(value)?;
    if dry {
      return send_completed(&writer, id, 0, records::dry_run_outputs()).await;
    }
    if cancel_token.is_cancelled() {
      return send_cancelled(&writer, id).await;
    }

    // Compatibility is established before prompt files, credentials, schema
    // files, or run-record directories are touched. There is deliberately no
    // PATH or shell fallback.
    let executable = match invocation::CodexExecutable::resolve_from_operator_environment(&cancel_token).await {
      Ok(executable) => executable,
      Err(_) if cancel_token.is_cancelled() => return send_cancelled(&writer, id).await,
      Err(error) => return Err(error),
    };
    let structured_result = if config.result_schema.is_some() {
      invocation::StructuredResultTarget::SchemaFile(records::RunRecords::schema_path_for(
        &dir,
        &config.run_records,
        &id,
      ))
    } else {
      invocation::StructuredResultTarget::Disabled
    };
    let sanitizer = sanitization::RunSanitizer::from_variables(&vars, &secret_vars);
    let invocation = invocation::CodexInvocation::load(
      executable,
      &config,
      &dir,
      structured_result,
      invocation::EnvironmentSources {
        variables: &vars,
        secret_variables: &secret_vars,
        task_environment: &envs,
      },
    )
    .await?;
    let mut run_records = records::RunRecords::create(&dir, &config.run_records, &id).await?;
    run_records.materialize_schema(invocation.result_schema()).await?;
    if cancel_token.is_cancelled() {
      return send_cancelled(&writer, id).await;
    }

    match command::run(
      &id,
      &invocation,
      &dir,
      writer.clone(),
      &sanitizer,
      run_records.trace(),
      &cancel_token,
    )
    .await?
    {
      command::CommandOutcome::Cancelled => send_cancelled(&writer, id).await,
      command::CommandOutcome::Completed(completion) => {
        let normalized = records::normalize_terminal(completion.terminal_event(), config.result_schema.as_ref())?;
        let committed = run_records.commit(normalized, &config, &invocation, &sanitizer).await?;
        resources::publish(&writer, &id, &dir, &committed.paths, &config.deliverables).await?;
        send_completed(&writer, id, 0, committed.outputs).await
      },
    }
  }
}

async fn send_cancelled(
  writer: &Arc<Mutex<impl AsyncWrite + Send + 'static + Unpin>>,
  id: String,
) -> anyhow::Result<()> {
  send_completed(writer, id, -1, Default::default()).await
}

async fn send_completed(
  writer: &Arc<Mutex<impl AsyncWrite + Send + 'static + Unpin>>,
  id: String,
  code: i32,
  outputs: serde_json::Map<String, serde_json::Value>,
) -> anyhow::Result<()> {
  if code == 0 {
    records::validate_outputs(&outputs)?;
  }
  send_response(writer, &PluginResponse::Completed { id, code, outputs }).await?;
  Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  serve_plugin(CodexPlugin, plugin_schema()).await
}

#[cfg(test)]
#[path = "plugin_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "resources_tests.rs"]
mod resources_tests;
