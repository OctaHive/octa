use std::{ffi::OsString, fs};

use super::*;
use serde_json::json;

fn config(value: serde_json::Value) -> CodexConfig {
  CodexConfig::parse(value).unwrap()
}

async fn load_invocation(
  config: &CodexConfig,
  workspace: &Path,
  structured_result: StructuredResultTarget,
) -> anyhow::Result<CodexInvocation> {
  let variables = HashMap::new();
  let task_environment = HashMap::new();
  CodexInvocation::load(
    CodexExecutable::fixture(),
    config,
    workspace,
    structured_result,
    EnvironmentSources {
      variables: &variables,
      secret_variables: &[],
      task_environment: &task_environment,
    },
  )
  .await
}

async fn load_with_environment(
  config: &CodexConfig,
  workspace: &Path,
  variables: &HashMap<String, Value>,
  secret_variables: &[String],
  task_environment: &HashMap<String, String>,
) -> anyhow::Result<CodexInvocation> {
  CodexInvocation::load(
    CodexExecutable::fixture(),
    config,
    workspace,
    StructuredResultTarget::Disabled,
    EnvironmentSources {
      variables,
      secret_variables,
      task_environment,
    },
  )
  .await
}

#[tokio::test]
async fn inline_unicode_prompt_has_a_stable_identity_and_is_delivered_only_over_stdin() {
  let workspace = tempfile::tempdir().unwrap();
  let prompt = "Проверь 🦀-проект и верни JSON.";
  let config = config(json!({
    "prompt": prompt,
    "model": "gpt-codex",
    "reasoning_effort": "high"
  }));
  let invocation = load_invocation(&config, workspace.path(), StructuredResultTarget::Disabled)
    .await
    .unwrap();

  assert_eq!(invocation.prompt_identity(), blake3::hash(prompt.as_bytes()));
  assert_eq!(invocation.executable().version(), "0.130.0");
  assert!(invocation.executable().path().is_absolute());
  assert_eq!(
    invocation.arguments(),
    [
      "exec",
      "--json",
      "--sandbox=workspace-write",
      "--ask-for-approval=never",
      "--model=gpt-codex",
      "--config=model_reasoning_effort=\"high\"",
      "-",
    ]
    .map(OsString::from)
  );
  assert!(invocation.arguments().iter().all(|argument| argument != prompt));

  assert_eq!(invocation.prompt_bytes(), prompt.as_bytes());
}

#[tokio::test]
async fn prompt_file_uses_the_same_bytes_and_identity_as_an_inline_prompt() {
  let workspace = tempfile::tempdir().unwrap();
  fs::create_dir(workspace.path().join("prompts")).unwrap();
  let prompt = "Explain café.rs without changing it.\n";
  fs::write(workspace.path().join("prompts/review.md"), prompt).unwrap();

  let from_file = load_invocation(
    &config(json!({ "prompt_file": "prompts/review.md" })),
    workspace.path(),
    StructuredResultTarget::Disabled,
  )
  .await
  .unwrap();
  let inline = load_invocation(
    &config(json!({ "prompt": prompt })),
    workspace.path(),
    StructuredResultTarget::Disabled,
  )
  .await
  .unwrap();

  assert_eq!(from_file.prompt_identity(), inline.prompt_identity());
  assert_eq!(from_file.prompt_bytes(), prompt.as_bytes());
}

#[tokio::test]
async fn prompt_file_loading_enforces_size_utf8_and_regular_file_boundaries() {
  let workspace = tempfile::tempdir().unwrap();
  fs::write(workspace.path().join("largest.txt"), vec![b'x'; MAX_PROMPT_BYTES]).unwrap();
  fs::write(workspace.path().join("oversized.txt"), vec![b'x'; MAX_PROMPT_BYTES + 1]).unwrap();
  fs::write(workspace.path().join("invalid.txt"), [0xff]).unwrap();
  fs::write(workspace.path().join("empty.txt"), []).unwrap();
  fs::write(workspace.path().join("contains-nul.txt"), b"prompt\0suffix").unwrap();
  fs::create_dir(workspace.path().join("directory")).unwrap();

  assert!(load_invocation(
    &config(json!({ "prompt_file": "largest.txt" })),
    workspace.path(),
    StructuredResultTarget::Disabled,
  )
  .await
  .is_ok());
  for path in [
    "oversized.txt",
    "invalid.txt",
    "empty.txt",
    "contains-nul.txt",
    "directory",
  ] {
    assert!(
      load_invocation(
        &config(json!({ "prompt_file": path })),
        workspace.path(),
        StructuredResultTarget::Disabled,
      )
      .await
      .is_err(),
      "accepted {path}"
    );
  }
}

#[tokio::test]
async fn structured_result_arguments_are_typed_and_never_accept_pass_through_flags() {
  let workspace = tempfile::tempdir().unwrap();
  let prompt = "--dangerously-bypass-approvals-and-sandbox";
  let config = config(json!({
    "prompt": prompt,
    "model": "--untrusted-model-shaped-flag",
    "result_schema": {
      "type": "object",
      "properties": { "summary": { "type": "string" } },
      "required": ["summary"],
      "additionalProperties": false
    }
  }));
  let schema_path = workspace.path().join("internal/result-schema.json");
  let invocation = load_invocation(
    &config,
    workspace.path(),
    StructuredResultTarget::SchemaFile(schema_path.clone()),
  )
  .await
  .unwrap();

  assert_eq!(
    invocation.arguments(),
    [
      OsString::from("exec"),
      OsString::from("--json"),
      OsString::from("--sandbox=workspace-write"),
      OsString::from("--ask-for-approval=never"),
      OsString::from("--model=--untrusted-model-shaped-flag"),
      {
        let mut value = OsString::from("--output-schema=");
        value.push(&schema_path);
        value
      },
      OsString::from("-"),
    ]
  );
  assert!(!invocation.arguments().contains(&OsString::from(prompt)));
  assert!(!invocation
    .arguments()
    .contains(&OsString::from("--untrusted-model-shaped-flag")));

  let schema = invocation.result_schema().unwrap();
  assert_eq!(schema.path(), schema_path);
  let value: serde_json::Value = serde_json::from_slice(schema.bytes()).unwrap();
  assert_eq!(value, json!(config.result_schema));
}

#[tokio::test]
async fn schema_configuration_and_internal_target_must_agree() {
  let workspace = tempfile::tempdir().unwrap();
  let configured = config(json!({ "prompt": "work", "result_schema": { "type": "string" } }));
  assert!(
    load_invocation(&configured, workspace.path(), StructuredResultTarget::Disabled)
      .await
      .is_err()
  );
  assert!(load_invocation(
    &configured,
    workspace.path(),
    StructuredResultTarget::SchemaFile(PathBuf::new()),
  )
  .await
  .is_err());

  let plain = config(json!({ "prompt": "work" }));
  assert!(load_invocation(
    &plain,
    workspace.path(),
    StructuredResultTarget::SchemaFile(workspace.path().join("schema.json")),
  )
  .await
  .is_err());
}

#[tokio::test]
async fn internal_prompt_source_invariant_is_checked_again_at_the_io_boundary() {
  let workspace = tempfile::tempdir().unwrap();
  let mut invalid = config(json!({ "prompt": "work" }));
  invalid.prompt = None;
  assert!(
    load_invocation(&invalid, workspace.path(), StructuredResultTarget::Disabled)
      .await
      .err()
      .unwrap()
      .to_string()
      .contains("invalid prompt source")
  );
}

#[tokio::test]
async fn every_reasoning_effort_has_one_fixed_config_argument() {
  let workspace = tempfile::tempdir().unwrap();
  for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
    let invocation = load_invocation(
      &config(json!({ "prompt": "work", "reasoning_effort": effort })),
      workspace.path(),
      StructuredResultTarget::Disabled,
    )
    .await
    .unwrap();
    assert!(invocation
      .arguments()
      .contains(&OsString::from(format!("--config=model_reasoning_effort=\"{effort}\""))));
  }
}

#[tokio::test]
async fn bounded_reader_rejects_content_that_outgrows_earlier_metadata() {
  let bytes = vec![b'x'; MAX_PROMPT_BYTES + 1];
  let error = read_bounded_prompt(bytes.as_slice(), "growing.txt", 1)
    .await
    .unwrap_err();
  assert!(error.to_string().contains("exceeds"));
}

#[tokio::test]
async fn child_environment_contains_only_the_baseline_and_explicit_scalar_mappings() {
  let workspace = tempfile::tempdir().unwrap();
  let mut task_environment = PLATFORM_ENVIRONMENT
    .iter()
    .map(|name| ((*name).to_owned(), format!("baseline-{name}")))
    .collect::<HashMap<_, _>>();
  task_environment.insert("AMBIENT_PUBLIC".to_owned(), "must-not-be-inherited".to_owned());
  task_environment.insert("OPENAI_API_KEY".to_owned(), "ambient-secret".to_owned());

  let variables = HashMap::from([
    ("public_text".to_owned(), json!("visible")),
    ("public_number".to_owned(), json!(3)),
    ("public_boolean".to_owned(), json!(true)),
    ("tool_path".to_owned(), json!("selected-path")),
    ("credential".to_owned(), json!("selected-secret")),
    ("unused_credential".to_owned(), json!("unselected-secret")),
  ]);
  let secret_variables = vec!["credential".to_owned(), "unused_credential".to_owned()];
  let config = config(json!({
    "prompt": "work",
    "environment": {
      "public": {
        "VISIBLE": "public_text",
        "ATTEMPTS": "public_number",
        "ENABLED": "public_boolean",
        "path": "tool_path"
      },
      "secret": { "OPENAI_API_KEY": "credential" }
    }
  }));

  let invocation = load_with_environment(
    &config,
    workspace.path(),
    &variables,
    &secret_variables,
    &task_environment,
  )
  .await
  .unwrap();
  let environment = invocation.environment();

  for name in PLATFORM_ENVIRONMENT {
    let expected = if *name == "PATH" {
      "selected-path".to_owned()
    } else {
      format!("baseline-{name}")
    };
    assert_eq!(environment.get(*name), Some(&expected));
  }
  assert!(!environment.contains_key("path"));
  assert_eq!(environment.get("VISIBLE").map(String::as_str), Some("visible"));
  assert_eq!(environment.get("ATTEMPTS").map(String::as_str), Some("3"));
  assert_eq!(environment.get("ENABLED").map(String::as_str), Some("true"));
  assert_eq!(
    environment.get("OPENAI_API_KEY").map(String::as_str),
    Some("selected-secret")
  );
  assert!(!environment.contains_key("AMBIENT_PUBLIC"));
  assert!(environment.values().all(|value| value != "ambient-secret"));
  assert!(environment.values().all(|value| value != "unselected-secret"));
}

#[tokio::test]
async fn child_environment_rejects_mismatched_secret_classification() {
  let workspace = tempfile::tempdir().unwrap();
  let variables = HashMap::from([
    ("ordinary".to_owned(), json!("public")),
    ("credential".to_owned(), json!("do-not-print")),
  ]);
  let task_environment = HashMap::new();

  let secret_from_public = config(json!({
    "prompt": "work",
    "environment": { "secret": { "OPENAI_API_KEY": "ordinary" } }
  }));
  let error = load_with_environment(
    &secret_from_public,
    workspace.path(),
    &variables,
    &["credential".to_owned()],
    &task_environment,
  )
  .await
  .err()
  .unwrap();
  assert!(error.to_string().contains("not marked secret"));

  let public_from_secret = config(json!({
    "prompt": "work",
    "environment": { "public": { "VISIBLE": "credential" } }
  }));
  let error = load_with_environment(
    &public_from_secret,
    workspace.path(),
    &variables,
    &["credential".to_owned()],
    &task_environment,
  )
  .await
  .err()
  .unwrap();
  assert!(error.to_string().contains("references secret variable"));
  assert!(!error.to_string().contains("do-not-print"));
}

#[tokio::test]
async fn child_environment_rejects_missing_structured_and_nul_values_without_echoing_them() {
  let workspace = tempfile::tempdir().unwrap();
  let task_environment = HashMap::new();
  for (variable_name, variables, expected) in [
    ("missing", HashMap::new(), "is not defined"),
    (
      "structured",
      HashMap::from([("structured".to_owned(), json!({ "token": "hidden" }))]),
      "must resolve to a scalar value",
    ),
    (
      "contains_nul",
      HashMap::from([("contains_nul".to_owned(), json!("secret\0suffix"))]),
      "must not contain NUL",
    ),
  ] {
    let config = config(json!({
      "prompt": "work",
      "environment": { "secret": { "OPENAI_API_KEY": variable_name } }
    }));
    let error = load_with_environment(
      &config,
      workspace.path(),
      &variables,
      &[variable_name.to_owned()],
      &task_environment,
    )
    .await
    .err()
    .unwrap();
    let message = error.to_string();
    assert!(message.contains(expected), "unexpected error: {message}");
    assert!(!message.contains("hidden"));
    assert!(!message.contains("secret\0suffix"));
  }
}

#[tokio::test]
async fn platform_baseline_cannot_smuggle_an_unselected_secret() {
  let workspace = tempfile::tempdir().unwrap();
  let variables = HashMap::from([("credential".to_owned(), json!("unselected-secret"))]);
  let secret_variables = vec!["credential".to_owned()];
  let task_environment = HashMap::from([("PATH".to_owned(), "/bin/unselected-secret/tools".to_owned())]);
  let config = config(json!({ "prompt": "work" }));

  let error = load_with_environment(
    &config,
    workspace.path(),
    &variables,
    &secret_variables,
    &task_environment,
  )
  .await
  .err()
  .unwrap();
  let message = error.to_string();
  assert!(message.contains("requires an explicit secret mapping"));
  assert!(!message.contains("unselected-secret"));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_rejects_ambiguous_case_variants_in_the_task_environment() {
  let workspace = tempfile::tempdir().unwrap();
  let task_environment = HashMap::from([
    ("PATH".to_owned(), r"C:\tools".to_owned()),
    ("Path".to_owned(), r"C:\other-tools".to_owned()),
  ]);
  let error = load_with_environment(
    &config(json!({ "prompt": "work" })),
    workspace.path(),
    &HashMap::new(),
    &[],
    &task_environment,
  )
  .await
  .err()
  .unwrap();
  assert!(error.to_string().contains("differing only by ASCII case"));
}

#[cfg(unix)]
#[tokio::test]
async fn prompt_file_must_not_be_a_symlink_or_escape_the_task_directory() {
  use std::os::unix::fs::symlink;

  let parent = tempfile::tempdir().unwrap();
  let workspace = parent.path().join("workspace");
  fs::create_dir(&workspace).unwrap();
  fs::write(parent.path().join("outside.txt"), "outside").unwrap();
  symlink(parent.path().join("outside.txt"), workspace.join("prompt.txt")).unwrap();

  let error = load_invocation(
    &config(json!({ "prompt_file": "prompt.txt" })),
    &workspace,
    StructuredResultTarget::Disabled,
  )
  .await
  .err()
  .unwrap();
  assert!(error.to_string().contains("symbolic link"));

  let outside = parent.path().join("outside");
  fs::create_dir(&outside).unwrap();
  fs::write(outside.join("prompt.txt"), "outside").unwrap();
  symlink(&outside, workspace.join("linked-directory")).unwrap();
  let error = load_invocation(
    &config(json!({ "prompt_file": "linked-directory/prompt.txt" })),
    &workspace,
    StructuredResultTarget::Disabled,
  )
  .await
  .err()
  .unwrap();
  assert!(error.to_string().contains("safely beneath the task directory"));
}

#[cfg(unix)]
#[test]
fn opened_workspace_capability_cannot_be_redirected_by_replacing_its_path() {
  use std::{io::Read, os::unix::fs::symlink};

  let parent = tempfile::tempdir().unwrap();
  let workspace = parent.path().join("workspace");
  let moved_workspace = parent.path().join("workspace-opened");
  let outside = parent.path().join("outside");
  fs::create_dir(&workspace).unwrap();
  fs::create_dir(&outside).unwrap();
  fs::write(workspace.join("prompt.txt"), "inside").unwrap();
  fs::write(outside.join("prompt.txt"), "outside").unwrap();

  let root = Dir::open_ambient_dir(&workspace, ambient_authority()).unwrap();
  fs::rename(&workspace, &moved_workspace).unwrap();
  symlink(&outside, &workspace).unwrap();

  let (mut file, _) = open_prompt_from_directory(&root, "prompt.txt").unwrap();
  let mut prompt = String::new();
  file.read_to_string(&mut prompt).unwrap();
  assert_eq!(prompt, "inside");
}
