use std::{collections::HashMap, fs, path::Path};

use octa_plugin::{
  logger::{Logger, MockLogger, REDACTION_MARKER},
  protocol::{PluginCachePlanRequest, PluginResponse, TargetPlatform, MAX_PLUGIN_FRAME_BYTES},
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader, DuplexStream};

use super::*;

fn command(workspace: &Path, value: Value, vars: HashMap<String, Value>) -> PluginCommand {
  PluginCommand {
    id: "command-id".to_owned(),
    dry: true,
    value,
    args: Vec::new(),
    dir: workspace.to_owned(),
    vars,
    secret_vars: vec!["credential".to_owned()],
    envs: HashMap::from([("PATH".to_owned(), workspace.join("missing-bin").display().to_string())]),
    raw: false,
    input: tokio::sync::mpsc::unbounded_channel().1,
  }
}

async fn execute(command: PluginCommand) -> anyhow::Result<Vec<PluginResponse>> {
  execute_with_cancellation(command, CancellationToken::new()).await
}

async fn execute_with_cancellation(
  command: PluginCommand,
  cancellation: CancellationToken,
) -> anyhow::Result<Vec<PluginResponse>> {
  let (stream, mut reader) = tokio::io::duplex(4096);
  CodexPlugin
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

async fn forward_event_chunk(
  decoder: &mut events::EventDecoder,
  sanitizer: &sanitization::RunSanitizer,
  writer: &Arc<Mutex<DuplexStream>>,
  mut bytes: &[u8],
) -> anyhow::Result<()> {
  while !bytes.is_empty() {
    let Some(event) = decoder.next_event(&mut bytes)? else {
      continue;
    };
    let event = sanitizer.sanitize_event(event)?;
    for response in events::normalize_activity("command-id", &event)? {
      send_response(writer, &response).await?;
    }
  }
  Ok(())
}

#[test]
fn exposes_the_codex_task_contract_without_raw_mode() {
  let schema = plugin_schema();

  assert_eq!(schema.key, "codex");
  assert!(!schema.supports_raw);
  assert!(schema.capabilities.is_empty());
  assert!(schema.input_schema.is_some());
  assert!(schema.output_schema.is_some());
}

#[test]
fn exposes_the_package_version() {
  assert_eq!(CodexPlugin.version(), env!("CARGO_PKG_VERSION"));
}

#[test]
fn reports_no_automatic_cache_contract() {
  let request = PluginCachePlanRequest {
    params: json!({
      "prompt_file": "missing/prompt.md",
      "deliverables": [{ "kind": "artifact", "name": "patch", "path": "out/patch.diff" }]
    }),
    working_directory: "project".to_owned(),
    target: TargetPlatform {
      os: "linux".to_owned(),
      architecture: "x86_64".to_owned(),
    },
  };

  assert!(CodexPlugin.cache_plan(&request).unwrap().is_none());
}

#[tokio::test]
async fn cancellation_before_spawn_returns_one_terminal_response_without_resolving_codex() {
  let workspace = tempfile::tempdir().unwrap();
  let mut request = command(workspace.path(), json!({ "prompt": "do not run" }), HashMap::new());
  request.dry = false;
  let cancellation = CancellationToken::new();
  cancellation.cancel();

  let responses = execute_with_cancellation(request, cancellation).await.unwrap();

  assert!(matches!(
    responses.as_slice(),
    [PluginResponse::Completed {
      id,
      code: -1,
      outputs,
    }] if id == "command-id" && outputs.is_empty()
  ));
  assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn dry_run_validates_without_reading_execution_material_or_mutating_the_workspace() {
  let workspace = tempfile::tempdir().unwrap();
  let secret = "must-not-appear";
  let responses = execute(command(
    workspace.path(),
    json!({
      "prompt_file": "missing/prompt.md",
      "run_records": ".octa/codex-runs",
      "environment": { "secret": { "OPENAI_API_KEY": "credential" } }
    }),
    HashMap::from([("credential".to_owned(), json!(secret))]),
  ))
  .await
  .unwrap();

  let [PluginResponse::Completed { id, code: 0, outputs }] = responses.as_slice() else {
    panic!("unexpected dry-run responses: {responses:?}");
  };
  assert_eq!(id, "command-id");
  assert_eq!(outputs, &records::dry_run_outputs());
  assert!(jsonschema::validator_for(&Value::Object(records::output_schema()))
    .unwrap()
    .is_valid(&Value::Object(outputs.clone())));
  assert!(!format!("{responses:?}").contains(secret));
  assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn dry_run_rejects_an_invalid_static_configuration_before_execution() {
  let workspace = tempfile::tempdir().unwrap();
  let error = execute(command(
    workspace.path(),
    json!({ "prompt": "inline", "prompt_file": "prompt.md" }),
    HashMap::new(),
  ))
  .await
  .unwrap_err();

  assert!(error.to_string().contains("exactly one of prompt or prompt_file"));
  assert!(fs::read_dir(workspace.path()).unwrap().next().is_none());
}

#[tokio::test]
async fn transport_rejects_a_response_the_host_cannot_read() {
  let (writer, _reader) = tokio::io::duplex(64);
  let writer = Arc::new(Mutex::new(writer));
  let response = PluginResponse::Stdout {
    id: "command-id".to_owned(),
    line: "x".repeat(MAX_PLUGIN_FRAME_BYTES),
  };

  let error = send_response(&writer, &response).await.unwrap_err();

  assert!(error.to_string().contains("frame limit"));
}

#[tokio::test]
async fn normalized_activity_is_forwarded_before_the_terminal_frame_is_received() {
  let (stream, reader) = tokio::io::duplex(16 * 1024);
  let writer = Arc::new(Mutex::new(stream));
  let mut decoder = events::EventDecoder::new();
  let sanitizer = sanitization::RunSanitizer::from_variables(&HashMap::new(), &[]);
  forward_event_chunk(
    &mut decoder,
    &sanitizer,
    &writer,
    concat!(
      "{\"type\":\"turn.started\"}\n",
      "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"working\"}}\n"
    )
    .as_bytes(),
  )
  .await
  .unwrap();

  let mut reader = BufReader::new(reader);
  let mut frame = String::new();
  reader.read_line(&mut frame).await.unwrap();
  assert!(matches!(
    serde_json::from_str::<PluginResponse>(&frame).unwrap(),
    PluginResponse::Progress { .. }
  ));
  frame.clear();
  reader.read_line(&mut frame).await.unwrap();
  assert!(matches!(
    serde_json::from_str::<PluginResponse>(&frame).unwrap(),
    PluginResponse::Stdout { line, .. } if line == "working"
  ));

  forward_event_chunk(
    &mut decoder,
    &sanitizer,
    &writer,
    b"{\"type\":\"turn.completed\",\"usage\":{}}\n",
  )
  .await
  .unwrap();
  assert!(decoder.finish().unwrap().is_none());
  send_completed(&writer, "command-id".to_owned(), 0, records::dry_run_outputs())
    .await
    .unwrap();
  frame.clear();
  reader.read_line(&mut frame).await.unwrap();
  assert!(matches!(
    serde_json::from_str::<PluginResponse>(&frame).unwrap(),
    PluginResponse::Completed { code: 0, .. }
  ));
}

#[tokio::test]
async fn selected_secrets_never_cross_event_output_or_record_boundaries() {
  const STRING_SECRET: &str = "adversarial-secret-token";
  const NESTED_SECRET: &str = "nested-secret-value";
  const NUMBER_SECRET: i64 = 987_654_321;

  let variables = HashMap::from([
    (
      "credential".to_owned(),
      json!({
        "token": STRING_SECRET,
        "nested": [NESTED_SECRET, NUMBER_SECRET, false]
      }),
    ),
    ("ordinary".to_owned(), json!("visible-value")),
  ]);
  let sanitizer = sanitization::RunSanitizer::from_variables(&variables, &["credential".to_owned()]);
  let mut decoder = events::EventDecoder::new();
  let mut stream = Vec::new();
  for event in [
    json!({
      "type": "item.completed",
      "item": {"type": "agent_message", "text": format!("stdout {STRING_SECRET}")}
    }),
    json!({
      "type": "item.completed",
      "item": {
        "type": "command_execution",
        "aggregated_output": format!("stderr {NESTED_SECRET}"),
        "status": "failed"
      }
    }),
    json!({"type": "error", "message": format!("diagnostic {STRING_SECRET}")}),
    json!({
      "type": "turn.completed",
      "result": {
        "secret": STRING_SECRET,
        "number": NUMBER_SECRET,
        "flag": false,
        "public": "visible-value"
      }
    }),
  ] {
    serde_json::to_writer(&mut stream, &event).unwrap();
    stream.push(b'\n');
  }
  let (writer, mut reader) = tokio::io::duplex(16 * 1024);
  let writer = Arc::new(Mutex::new(writer));
  let logger = MockLogger::new();
  let mut trace = Vec::new();
  let mut input = stream.as_slice();
  while !input.is_empty() {
    let Some(event) = decoder.next_event(&mut input).unwrap() else {
      continue;
    };
    let event = sanitizer.sanitize_event(event).unwrap();
    serde_json::to_writer(&mut trace, event.value()).unwrap();
    trace.push(b'\n');
    logger.log(&serde_json::to_string(event.value()).unwrap()).unwrap();
    for response in events::normalize_activity("command-id", &event).unwrap() {
      send_response(&writer, &response).await.unwrap();
    }
  }
  assert!(decoder.finish().unwrap().is_none());
  send_response(
    &writer,
    &PluginResponse::Stderr {
      id: "command-id".to_owned(),
      line: sanitizer.sanitize_text(&format!("raw stderr {STRING_SECRET}")),
    },
  )
  .await
  .unwrap();

  let structured_result = sanitizer
    .sanitize_value(json!({
      "message": format!("result {STRING_SECRET}"),
      "nested": [NESTED_SECRET, NUMBER_SECRET, false],
      "public": "visible-value"
    }))
    .unwrap();
  let outputs = json!({
    "outcome": "failed",
    "structured_result": structured_result,
    "harness_identifiers": {},
    "usage": {},
    "record_paths": {
      "trace": ".octa/codex-runs/command-id/trace.jsonl",
      "result": ".octa/codex-runs/command-id/result.json",
      "provenance": ".octa/codex-runs/command-id/provenance.json"
    }
  })
  .as_object()
  .cloned()
  .unwrap();
  send_completed(&writer, "command-id".to_owned(), 0, outputs)
    .await
    .unwrap();
  drop(writer);

  let mut protocol = Vec::new();
  reader.read_to_end(&mut protocol).await.unwrap();
  let trace = String::from_utf8(trace).unwrap();
  let protocol = String::from_utf8(protocol).unwrap();
  let logs = logger.get_messages().join("\n");
  for sink in [&trace, &protocol, &logs] {
    assert!(!sink.contains(STRING_SECRET));
    assert!(!sink.contains(NESTED_SECRET));
    assert!(!sink.contains(&NUMBER_SECRET.to_string()));
    assert!(!sink.contains(":false"));
    assert!(sink.contains(REDACTION_MARKER));
    assert!(sink.contains("visible-value"));
  }
}
