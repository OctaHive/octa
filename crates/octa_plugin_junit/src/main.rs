use std::{env, path::PathBuf, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use octa_plugin::{
  logger::Logger,
  protocol::{PluginResponse, ReportDeclaration},
  serve_plugin, Plugin, PluginCommand, PluginSchema,
};
use serde::Deserialize;
use tokio::{
  io::{AsyncWrite, AsyncWriteExt},
  sync::Mutex,
};
use tokio_util::sync::CancellationToken;

struct JunitPlugin;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JunitParams {
  name: String,
  path: PathBuf,
}

fn plugin_schema() -> PluginSchema {
  PluginSchema {
    key: "junit".to_owned(),
    supports_raw: false,
    capabilities: Vec::new(),
    input_schema: serde_json::json!({
      "type": "object",
      "properties": {
        "name": { "type": "string", "minLength": 1 },
        "path": { "type": "string", "minLength": 1 }
      },
      "required": ["name", "path"],
      "additionalProperties": false
    })
    .as_object()
    .cloned(),
    output_schema: None,
  }
}

async fn send<W>(writer: &Arc<Mutex<W>>, response: &PluginResponse) -> anyhow::Result<()>
where
  W: AsyncWrite + Send + Unpin,
{
  let mut message = serde_json::to_vec(response)?;
  message.push(b'\n');
  let mut writer = writer.lock().await;
  writer.write_all(&message).await?;
  writer.flush().await?;
  Ok(())
}

#[async_trait]
impl Plugin for JunitPlugin {
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  async fn execute_command(
    &self,
    request: PluginCommand,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + Unpin>>,
    logger: Arc<impl Logger>,
    cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    let params: JunitParams = serde_json::from_str(&request.command).context("invalid JUnit parameters")?;

    if cancel_token.is_cancelled() {
      return send(
        &writer,
        &PluginResponse::Completed {
          id: request.id,
          code: -1,
          outputs: Default::default(),
        },
      )
      .await;
    }

    if !request.dry {
      let response = PluginResponse::RegisterReport {
        id: request.id.clone(),
        report: ReportDeclaration {
          name: params.name,
          path: params.path,
          format: "junit".to_owned(),
        },
      };
      logger.log("Register JUnit report")?;
      send(&writer, &response).await?;
    }

    send(
      &writer,
      &PluginResponse::Completed {
        id: request.id,
        code: 0,
        outputs: Default::default(),
      },
    )
    .await
  }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  serve_plugin(JunitPlugin, plugin_schema()).await
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use octa_plugin::logger::MockLogger;
  use tokio::io::{AsyncReadExt, DuplexStream};

  use super::*;

  fn command(command: &str, dry: bool) -> PluginCommand {
    PluginCommand {
      id: "command-id".to_owned(),
      dry,
      command: command.to_owned(),
      args: Vec::new(),
      dir: PathBuf::from("workspace"),
      vars: HashMap::new(),
      envs: HashMap::new(),
      raw: false,
      input: tokio::sync::mpsc::unbounded_channel().1,
    }
  }

  async fn execute(command: PluginCommand, cancellation: CancellationToken) -> anyhow::Result<Vec<PluginResponse>> {
    let (stream, mut reader) = tokio::io::duplex(4096);
    JunitPlugin
      .execute_command(
        command,
        Arc::new(Mutex::new(stream)),
        Arc::new(MockLogger::new()),
        cancellation,
      )
      .await?;
    read_responses(&mut reader).await
  }

  async fn read_responses(reader: &mut DuplexStream) -> anyhow::Result<Vec<PluginResponse>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    String::from_utf8(bytes)?
      .lines()
      .map(|line| serde_json::from_str(line).map_err(Into::into))
      .collect()
  }

  #[test]
  fn exposes_junit_task_schema() {
    let schema = plugin_schema();

    assert_eq!(schema.key, "junit");
    assert!(!schema.supports_raw);
    assert!(schema.capabilities.is_empty());
    assert_eq!(
      schema.input_schema.unwrap()["required"],
      serde_json::json!(["name", "path"])
    );
  }

  #[test]
  fn exposes_package_version() {
    assert_eq!(JunitPlugin.version(), env!("CARGO_PKG_VERSION"));
  }

  #[tokio::test]
  async fn registers_junit_report_then_completes() {
    let responses = execute(
      command(r#"{"name":"unit-tests","path":"reports/junit.xml"}"#, false),
      CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(matches!(
      &responses[0],
      PluginResponse::RegisterReport { id, report }
        if id == "command-id"
          && report.name == "unit-tests"
          && report.path.as_path() == std::path::Path::new("reports/junit.xml")
          && report.format == "junit"
    ));
    assert!(matches!(
      &responses[1],
      PluginResponse::Completed { id, code: 0, outputs } if id == "command-id" && outputs.is_empty()
    ));
  }

  #[tokio::test]
  async fn dry_run_does_not_register_a_missing_file() {
    let responses = execute(
      command(r#"{"name":"tests","path":"missing.xml"}"#, true),
      CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(matches!(
      responses.as_slice(),
      [PluginResponse::Completed { code: 0, .. }]
    ));
  }

  #[tokio::test]
  async fn cancellation_finishes_without_registering_report() {
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let responses = execute(command(r#"{"name":"tests","path":"report.xml"}"#, false), cancellation)
      .await
      .unwrap();

    assert!(matches!(
      responses.as_slice(),
      [PluginResponse::Completed { code: -1, .. }]
    ));
  }

  #[tokio::test]
  async fn rejects_invalid_parameters() {
    let (stream, _reader) = tokio::io::duplex(4096);
    let error = JunitPlugin
      .execute_command(
        command(r#"{"path":"report.xml","unknown":true}"#, false),
        Arc::new(Mutex::new(stream)),
        Arc::new(MockLogger::new()),
        CancellationToken::new(),
      )
      .await
      .unwrap_err();

    assert!(error.to_string().contains("invalid JUnit parameters"));
  }
}
