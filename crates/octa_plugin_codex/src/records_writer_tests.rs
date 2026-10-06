use std::{collections::HashMap, fs, path::Path};

use cap_std::{ambient_authority, fs::Dir};
use octa_plugin::logger::REDACTION_MARKER;
use serde_json::{json, Value};

use super::*;
use crate::{
  config::CodexConfig,
  contract::{RECORDS_DIRECTORY, RECORDS_STAGING_DIRECTORY},
  events::EventDecoder,
  invocation::{CodexExecutable, CodexInvocation, EnvironmentSources, StructuredResultTarget},
  sanitization::{RunSanitizer, SanitizedEvent},
};

fn sanitized_event(value: Value, sanitizer: &RunSanitizer) -> SanitizedEvent {
  let mut bytes = serde_json::to_vec(&value).unwrap();
  bytes.push(b'\n');
  let mut input = bytes.as_slice();
  let mut decoder = EventDecoder::new();
  let event = decoder.next_event(&mut input).unwrap().unwrap();
  sanitizer.sanitize_event(event).unwrap()
}

async fn invocation(
  config: &CodexConfig,
  workspace: &Path,
  target: StructuredResultTarget,
  variables: &HashMap<String, Value>,
  secret_variables: &[String],
) -> CodexInvocation {
  CodexInvocation::load(
    CodexExecutable::fixture(),
    config,
    workspace,
    target,
    EnvironmentSources {
      variables,
      secret_variables,
      task_environment: &HashMap::new(),
    },
  )
  .await
  .unwrap()
}

fn record_path(workspace: &Path, outputs: &serde_json::Map<String, Value>, name: &str) -> std::path::PathBuf {
  workspace.join(
    outputs["record_paths"][name]
      .as_str()
      .expect("record path must be a string"),
  )
}

#[tokio::test]
async fn writes_stable_sanitized_records_with_prompt_and_source_identity() {
  const SECRET: &str = "top-secret-credential";

  let workspace = tempfile::tempdir().unwrap();
  let config = CodexConfig::parse(json!({
    "prompt": "review the workspace",
    "model": SECRET,
    "reasoning_effort": "high",
    "source_revision": SECRET,
    "environment": { "secret": { "OPENAI_API_KEY": "credential" } }
  }))
  .unwrap();
  let variables = HashMap::from([("credential".to_owned(), json!(SECRET))]);
  let secret_variables = vec!["credential".to_owned()];
  let invocation = invocation(
    &config,
    workspace.path(),
    StructuredResultTarget::Disabled,
    &variables,
    &secret_variables,
  )
  .await;
  let sanitizer = RunSanitizer::from_variables(&variables, &secret_variables);
  let activity = sanitized_event(
    json!({"type": "item.completed", "item": {"type": "agent_message", "text": SECRET}}),
    &sanitizer,
  );
  let terminal = sanitized_event(
    json!({
      "type": "turn.completed",
      "message": "done",
      "thread_id": "thread-1",
      "turn_id": "turn-1",
      "usage": {"input_tokens": 11, "output_tokens": 7}
    }),
    &sanitizer,
  );

  let mut records = RunRecords::create(workspace.path(), &config.run_records, "command-a")
    .await
    .unwrap();
  let stable_directory = workspace
    .path()
    .join(records.relative_directory())
    .join(RECORDS_DIRECTORY);
  assert!(
    !stable_directory.exists(),
    "incomplete records became externally visible"
  );
  records.trace().append(&activity).await.unwrap();
  records.trace().append(&terminal).await.unwrap();
  let result = normalize_terminal(&terminal, None).unwrap();
  let outputs = records
    .commit(result, &config, &invocation, &sanitizer)
    .await
    .unwrap()
    .outputs;
  validate_outputs(&outputs).unwrap();
  assert!(stable_directory.is_dir(), "complete record set was not published");

  let trace = fs::read_to_string(record_path(workspace.path(), &outputs, "trace")).unwrap();
  let result = fs::read_to_string(record_path(workspace.path(), &outputs, "result")).unwrap();
  let provenance = fs::read_to_string(record_path(workspace.path(), &outputs, "provenance")).unwrap();
  let expected_result = concat!(
    "{\"format_version\":1,\"outcome\":\"completed\",\"final_message\":\"done\",",
    "\"harness_identifiers\":{\"thread_id\":\"thread-1\",\"turn_id\":\"turn-1\"},",
    "\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}\n"
  );
  assert_eq!(result, expected_result);
  assert!(trace.ends_with('\n'));
  assert_eq!(trace.lines().count(), 2);
  assert!(trace.contains(&format!("\"text\":\"{REDACTION_MARKER}\"")));

  let provenance_value: Value = serde_json::from_str(&provenance).unwrap();
  assert_eq!(provenance_value["format_version"], 1);
  assert_eq!(provenance_value["trace_format_version"], 1);
  assert_eq!(provenance_value["result_format_version"], 1);
  assert_eq!(provenance_value["source_revision"], REDACTION_MARKER);
  assert_eq!(provenance_value["settings"]["model"], REDACTION_MARKER);
  assert_eq!(provenance_value["settings"]["reasoning_effort"], "high");
  assert_eq!(provenance_value["prompt_digest"]["algorithm"], "blake3");
  assert_eq!(
    provenance_value["prompt_digest"]["value"],
    blake3::hash(b"review the workspace").to_hex().as_str()
  );
  assert!(provenance.ends_with('\n'));

  let all_records = format!("{trace}{result}{provenance}");
  for forbidden in [SECRET, "OPENAI_API_KEY", "credential", "environment"] {
    assert!(!all_records.contains(forbidden), "record leaked {forbidden:?}");
  }
}

#[tokio::test]
async fn omits_an_absent_source_revision_and_materializes_a_private_schema() {
  let workspace = tempfile::tempdir().unwrap();
  let config = CodexConfig::parse(json!({
    "prompt": "produce JSON",
    "result_schema": {
      "type": "object",
      "properties": {"answer": {"type": "string"}},
      "required": ["answer"]
    }
  }))
  .unwrap();
  let variables = HashMap::new();
  let secret_variables = Vec::new();
  let schema_path = RunRecords::schema_path_for(workspace.path(), &config.run_records, "structured");
  let invocation = invocation(
    &config,
    workspace.path(),
    StructuredResultTarget::SchemaFile(schema_path.clone()),
    &variables,
    &secret_variables,
  )
  .await;
  let mut records = RunRecords::create(workspace.path(), &config.run_records, "structured")
    .await
    .unwrap();
  records.materialize_schema(invocation.result_schema()).await.unwrap();
  assert!(schema_path.is_file());

  let sanitizer = RunSanitizer::from_variables(&variables, &secret_variables);
  let terminal = sanitized_event(
    json!({"type": "turn.completed", "result": {"answer": "yes"}}),
    &sanitizer,
  );
  records.trace().append(&terminal).await.unwrap();
  let result = normalize_terminal(&terminal, config.result_schema.as_ref()).unwrap();
  let outputs = records
    .commit(result, &config, &invocation, &sanitizer)
    .await
    .unwrap()
    .outputs;

  assert!(!schema_path.exists(), "private schema survived record commit");
  let provenance: Value =
    serde_json::from_slice(&fs::read(record_path(workspace.path(), &outputs, "provenance")).unwrap()).unwrap();
  assert!(provenance.get("source_revision").is_none());
  assert_eq!(outputs["structured_result"], json!({"answer": "yes"}));
}

#[tokio::test]
async fn concurrent_commands_are_isolated_and_reusing_an_id_is_refused() {
  let workspace = tempfile::tempdir().unwrap();
  let (first, second) = tokio::join!(
    RunRecords::create(workspace.path(), ".octa/codex-runs", "first"),
    RunRecords::create(workspace.path(), ".octa/codex-runs", "second")
  );
  let first = first.unwrap();
  let second = second.unwrap();
  assert_ne!(first.relative_directory(), second.relative_directory());
  assert!(workspace.path().join(first.relative_directory()).is_dir());
  assert!(workspace.path().join(second.relative_directory()).is_dir());

  let collision = RunRecords::create(workspace.path(), ".octa/codex-runs", "first")
    .await
    .err()
    .expect("a repeated command id must not overwrite existing records");
  assert!(collision.to_string().contains("already exists"));
}

#[tokio::test]
async fn failed_publication_removes_every_owned_partial_file() {
  let workspace = tempfile::tempdir().unwrap();
  let config = CodexConfig::parse(json!({"prompt": "work"})).unwrap();
  let variables = HashMap::new();
  let invocation = invocation(
    &config,
    workspace.path(),
    StructuredResultTarget::Disabled,
    &variables,
    &[],
  )
  .await;
  let sanitizer = RunSanitizer::from_variables(&variables, &[]);
  let terminal = sanitized_event(json!({"type": "turn.completed", "message": "done"}), &sanitizer);
  let mut records = RunRecords::create(workspace.path(), &config.run_records, "partial")
    .await
    .unwrap();
  let directory = workspace.path().join(records.relative_directory());
  let staging = workspace.path().join(records.staging_relative_directory());
  records.trace().append(&terminal).await.unwrap();
  fs::write(staging.join(".result.json.tmp"), b"collision").unwrap();

  let result = normalize_terminal(&terminal, None).unwrap();
  let error = records
    .commit(result, &config, &invocation, &sanitizer)
    .await
    .expect_err("exclusive temporary-file collision must fail");
  assert!(error.to_string().contains("temporary Codex run record"));
  assert!(!directory.exists(), "partial invocation directory survived failure");
}

#[tokio::test]
async fn refuses_invalid_storage_roots_and_schema_targets_without_partial_records() {
  let workspace = tempfile::tempdir().unwrap();
  let missing_workspace = workspace.path().join("missing");
  let error = RunRecords::create(&missing_workspace, ".octa/codex-runs", "missing")
    .await
    .err()
    .expect("a missing workspace must fail");
  assert!(error.to_string().contains("effective task directory"));

  fs::write(workspace.path().join("occupied"), b"not a directory").unwrap();
  let error = RunRecords::create(workspace.path(), "occupied/runs", "root-file")
    .await
    .err()
    .expect("a non-directory record root must fail");
  assert!(error.to_string().contains("run-record root"));

  let config = CodexConfig::parse(json!({
    "prompt": "produce JSON",
    "result_schema": {"type": "string"}
  }))
  .unwrap();
  let variables = HashMap::new();
  let wrong_target = workspace.path().join("wrong-schema.json");
  let wrong_invocation = invocation(
    &config,
    workspace.path(),
    StructuredResultTarget::SchemaFile(wrong_target),
    &variables,
    &[],
  )
  .await;
  let mut records = RunRecords::create(workspace.path(), &config.run_records, "wrong-target")
    .await
    .unwrap();
  let directory = workspace.path().join(records.relative_directory());
  let error = records
    .materialize_schema(wrong_invocation.result_schema())
    .await
    .expect_err("a schema target outside the invocation must fail");
  assert!(error.to_string().contains("does not belong"));
  drop(records);
  assert!(!directory.exists());

  let schema_path = RunRecords::schema_path_for(workspace.path(), &config.run_records, "occupied-schema");
  let correct_invocation = invocation(
    &config,
    workspace.path(),
    StructuredResultTarget::SchemaFile(schema_path.clone()),
    &variables,
    &[],
  )
  .await;
  let mut records = RunRecords::create(workspace.path(), &config.run_records, "occupied-schema")
    .await
    .unwrap();
  fs::write(&schema_path, b"occupied").unwrap();
  let error = records
    .materialize_schema(correct_invocation.result_schema())
    .await
    .expect_err("an existing private schema leaf must not be replaced");
  assert!(error.to_string().contains("temporary Codex run record"));
}

#[tokio::test]
async fn bounds_the_trace_and_refuses_a_stable_file_injected_before_publish() {
  let workspace = tempfile::tempdir().unwrap();
  let config = CodexConfig::parse(json!({"prompt": "work"})).unwrap();
  let variables = HashMap::new();
  let sanitizer = RunSanitizer::from_variables(&variables, &[]);
  let large_event = sanitized_event(
    json!({
      "type": "item.completed",
      "item": {"type": "agent_message", "text": "x".repeat(900 * 1024)}
    }),
    &sanitizer,
  );
  let mut records = RunRecords::create(workspace.path(), &config.run_records, "bounded-trace")
    .await
    .unwrap();
  let directory = workspace.path().join(records.relative_directory());
  let mut exceeded = None;
  for _ in 0..32 {
    if let Err(error) = records.trace().append(&large_event).await {
      exceeded = Some(error);
      break;
    }
  }
  assert!(
    exceeded.is_some_and(|error| error.to_string().contains("trace exceeds")),
    "trace budget was not enforced"
  );
  drop(records);
  assert!(!directory.exists(), "oversized trace survived as a partial record");

  let invocation = invocation(
    &config,
    workspace.path(),
    StructuredResultTarget::Disabled,
    &variables,
    &[],
  )
  .await;
  let terminal = sanitized_event(json!({"type": "turn.completed", "message": "done"}), &sanitizer);
  let mut records = RunRecords::create(workspace.path(), &config.run_records, "stable-collision")
    .await
    .unwrap();
  let directory = workspace.path().join(records.relative_directory());
  let staging = workspace.path().join(records.staging_relative_directory());
  records.trace().append(&terminal).await.unwrap();
  fs::write(staging.join("result.json"), b"injected").unwrap();
  let result = normalize_terminal(&terminal, None).unwrap();
  let error = records
    .commit(result, &config, &invocation, &sanitizer)
    .await
    .expect_err("a stable-name collision must not be overwritten");
  assert!(error.to_string().contains("atomically publish"));
  assert!(!directory.exists(), "failed publication left an invocation directory");
}

#[test]
fn filesystem_creation_rejects_invalid_components_and_creation_errors() {
  use crate::filesystem::create_directory_path_no_follow;

  let workspace = tempfile::tempdir().unwrap();
  let root = Dir::open_ambient_dir(workspace.path(), ambient_authority()).unwrap();
  for invalid in ["", ".", "..", "nested//child"] {
    assert_eq!(
      create_directory_path_no_follow(&root, invalid).unwrap_err().kind(),
      std::io::ErrorKind::InvalidInput
    );
  }
  assert_eq!(
    create_directory_path_no_follow(&root, "invalid\0component")
      .unwrap_err()
      .kind(),
    std::io::ErrorKind::InvalidInput
  );

  root.create_dir("not-a-file").unwrap();
  assert!(super::writer::remove_if_present(&root, "not-a-file").is_err());
  root.remove_dir("not-a-file").unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn cleanup_never_follows_a_replaced_staging_directory() {
  use std::os::unix::fs::symlink;

  let workspace = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  let records = RunRecords::create(workspace.path(), ".octa/codex-runs", "replaced-staging")
    .await
    .unwrap();
  let invocation = workspace.path().join(records.relative_directory());
  let staging = invocation.join(RECORDS_STAGING_DIRECTORY);
  fs::rename(&staging, invocation.join("displaced-staging")).unwrap();
  fs::write(outside.path().join("trace.jsonl"), b"must survive cleanup").unwrap();
  symlink(outside.path(), &staging).unwrap();

  drop(records);

  assert_eq!(
    fs::read(outside.path().join("trace.jsonl")).unwrap(),
    b"must survive cleanup"
  );
  assert!(staging.is_symlink(), "unsafe replacement was unexpectedly traversed");
}

#[cfg(unix)]
#[tokio::test]
async fn record_root_never_follows_a_workspace_symlink() {
  use std::os::unix::fs::symlink;

  let workspace = tempfile::tempdir().unwrap();
  let outside = tempfile::tempdir().unwrap();
  fs::create_dir(workspace.path().join("records-parent")).unwrap();
  symlink(outside.path(), workspace.path().join("records-parent/runs")).unwrap();

  let error = RunRecords::create(workspace.path(), "records-parent/runs", "linked-root")
    .await
    .err()
    .expect("a linked record root must fail closed");

  assert!(error.to_string().contains("safely open"));
  assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn publication_refuses_an_existing_stable_record_directory() {
  let workspace = tempfile::tempdir().unwrap();
  let config = CodexConfig::parse(json!({"prompt": "work"})).unwrap();
  let variables = HashMap::new();
  let invocation = invocation(
    &config,
    workspace.path(),
    StructuredResultTarget::Disabled,
    &variables,
    &[],
  )
  .await;
  let sanitizer = RunSanitizer::from_variables(&variables, &[]);
  let terminal = sanitized_event(json!({"type": "turn.completed", "message": "done"}), &sanitizer);
  let mut records = RunRecords::create(workspace.path(), &config.run_records, "stable-directory-collision")
    .await
    .unwrap();
  let invocation_directory = workspace.path().join(records.relative_directory());
  fs::create_dir(invocation_directory.join(RECORDS_DIRECTORY)).unwrap();
  records.trace().append(&terminal).await.unwrap();

  let result = normalize_terminal(&terminal, None).unwrap();
  let error = records
    .commit(result, &config, &invocation, &sanitizer)
    .await
    .expect_err("an existing stable record directory must not be replaced");

  assert!(error.to_string().contains("atomically publish"));
  assert!(
    !invocation_directory.exists(),
    "failed publication left a partial invocation directory"
  );
}
