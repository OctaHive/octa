//! Advisory runtime discovery, isolated from prompts, tasks and run records.
use crate::{config, invocation::CodexExecutable, process::ProcessTree};
use anyhow::{bail, Context};
use octa_plugin::protocol::{ModelCatalog, ModelChoice};
use serde_json::{json, Map, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::{sync::CancellationToken, task::AbortOnDropHandle};

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(10);
use octa_runner_protocol::{
  MAX_MODEL_CATALOG_BYTES as MAX_CATALOG_BYTES, MAX_MODEL_CATALOG_ENTRIES as MAX_MODELS, MAX_MODEL_ID_BYTES,
};

fn parameters() -> Map<String, Value> {
  let properties = config::input_schema()["properties"].as_object().unwrap().clone();
  json!({"type":"object", "additionalProperties":false,
    "properties":{"reasoning_effort":properties["reasoning_effort"]}})
  .as_object()
  .unwrap()
  .clone()
}

pub(crate) async fn discover(cancel: CancellationToken) -> ModelCatalog {
  let mut catalog = ModelCatalog {
    available: false,
    models: vec![],
    parameters: parameters(),
    manual_model: true,
  };
  let result = tokio::time::timeout(DISCOVERY_TIMEOUT, discover_models(&cancel)).await;
  if let Ok(Ok(models)) = result {
    catalog.models = models;
    catalog.available = true;
  }
  catalog
}

async fn discover_models(cancel: &CancellationToken) -> anyhow::Result<Vec<ModelChoice>> {
  let executable = CodexExecutable::resolve_from_operator_environment(cancel).await?;
  let mut command = executable.command().await?;
  command.arg("app-server").arg("--listen").arg("stdio://");
  // The plugin process is installation-owned. Discovery receives no task variables,
  // prompts, workspace or supplied environment overrides.
  let mut tree = ProcessTree::spawn(&mut command)?;
  let mut input = tree.take_stdin()?;
  let mut output = BufReader::new(tree.take_stdout()?);
  // Do not retain diagnostics that may describe private runtime configuration.
  let mut stderr = tree.take_stderr()?;
  let drain = AbortOnDropHandle::new(tokio::spawn(async move {
    tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await
  }));
  let operation = async {
    input
      .write_all(
        b"{\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"octa\",\"version\":\"1\"}}}\n",
      )
      .await?;
    let mut remaining = MAX_CATALOG_BYTES;
    reply(&mut output, 1, &mut remaining).await?;
    input.write_all(b"{\"method\":\"initialized\"}\n").await?;
    let mut cursor = Value::Null;
    let mut seen_cursors = std::collections::BTreeSet::new();
    let mut models = Vec::new();
    loop {
      let request =
        json!({"id":2,"method":"model/list","params":{"limit":MAX_MODELS,"cursor":cursor,"includeHidden":false}});
      input.write_all(format!("{request}\n").as_bytes()).await?;
      let page = reply(&mut output, 2, &mut remaining).await?;
      for model in page["data"].as_array().context("invalid model page")? {
        if model["hidden"] == true {
          continue;
        }
        let id = model["model"].as_str().context("missing model identifier")?;
        let label = model["displayName"].as_str().context("missing model label")?;
        if id.is_empty()
          || id.len() > MAX_MODEL_ID_BYTES
          || id.trim() != id
          || id.chars().any(char::is_control)
          || label.is_empty()
          || label.len() > 512
          || label.chars().any(char::is_control)
          || models.len() >= MAX_MODELS
          || models.iter().any(|m: &ModelChoice| m.id == id)
        {
          bail!("invalid model catalog");
        }
        let mut schema = parameters();
        let supported = config::input_schema()["properties"]["reasoning_effort"]["enum"]
          .as_array()
          .unwrap()
          .clone();
        let values: Vec<Value> = model["supportedReasoningEfforts"]
          .as_array()
          .context("missing reasoning metadata")?
          .iter()
          .map(|item| item["reasoningEffort"].clone())
          .filter(|value| supported.contains(value))
          .collect();
        if values.is_empty() {
          schema["properties"] = json!({});
        } else {
          schema["properties"]["reasoning_effort"]["enum"] = json!(values);
        }
        models.push(ModelChoice {
          id: id.into(),
          label: label.into(),
          parameters: schema,
        });
      }
      cursor = page["nextCursor"].clone();
      if cursor.is_null() {
        break;
      }
      let next = cursor
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 1024)
        .context("invalid model cursor")?;
      if !seen_cursors.insert(next.to_owned()) {
        bail!("repeated model cursor");
      }
    }
    Ok(models)
  };
  let result = tokio::select! {
    result = operation => result,
    _ = cancel.cancelled() => Err(anyhow::anyhow!("discovery cancelled")),
  };
  drop(input);
  let _ = tokio::time::timeout(Duration::from_secs(2), tree.terminate()).await;
  drain.abort();
  let _ = drain.await;
  result
}

async fn reply(
  reader: &mut BufReader<tokio::process::ChildStdout>,
  id: u64,
  remaining: &mut usize,
) -> anyhow::Result<Value> {
  loop {
    let mut line = Vec::new();
    let count = reader
      .take((*remaining + 1) as u64)
      .read_until(b'\n', &mut line)
      .await?;
    if count == 0 || count > *remaining {
      bail!("runtime catalog response exceeds its bound");
    }
    *remaining -= count;
    let value: Value = serde_json::from_slice(&line)?;
    if value["id"] != id {
      continue;
    }
    if value.get("error").is_some() {
      bail!("runtime catalog unavailable");
    }
    return value.get("result").cloned().context("missing runtime catalog result");
  }
}
