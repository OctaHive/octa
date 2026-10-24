use serde_json::{json, Value};

use super::*;

fn validator() -> jsonschema::Validator {
  jsonschema::validator_for(&Value::Object(input_schema())).expect("Codex input schema must compile")
}

#[test]
fn documented_configurations_match_the_advertised_contract() {
  const EXAMPLE_PREFIX: &str = "<!-- codex-config -->\n```yaml\n";
  let documentation = include_str!("../../../docs/codex-plugin.md").replace("\r\n", "\n");
  let mut examples = 0;

  for remainder in documentation.split(EXAMPLE_PREFIX).skip(1) {
    let source = remainder
      .split_once("\n```")
      .expect("documented Codex configuration must close its YAML block")
      .0;
    let document: Value = serde_yml::from_str(source).expect("documented Codex configuration must be valid YAML");
    let configuration = document
      .get("codex")
      .cloned()
      .expect("documented configuration must contain one codex value");
    assert!(
      validator().is_valid(&configuration),
      "documented configuration does not satisfy the plugin schema: {configuration}"
    );
    CodexConfig::parse(configuration).expect("documented configuration must satisfy semantic validation");
    examples += 1;
  }
  assert_eq!(examples, 2, "every documented Codex configuration must be validated");

  let schema = input_schema();
  let fields = schema["properties"]
    .as_object()
    .expect("Codex input schema properties must be an object");
  for field in fields.keys() {
    assert!(
      documentation.contains(&format!("| `{field}` |")),
      "Codex documentation omits the '{field}' configuration field"
    );
  }
}

fn assert_schema_and_parser_reject(value: Value) {
  assert!(!validator().is_valid(&value), "schema accepted {value}");
  assert!(CodexConfig::parse(value.clone()).is_err(), "parser accepted {value}");
}

#[test]
fn accepts_inline_and_complete_file_configurations() {
  let inline = json!({ "prompt": "Review the workspace." });
  assert!(validator().is_valid(&inline));
  let inline = CodexConfig::parse(inline).unwrap();
  assert_eq!(inline.prompt.as_deref(), Some("Review the workspace."));
  assert_eq!(inline.run_records, DEFAULT_RUN_RECORDS);
  assert!(inline.environment.public.is_empty());
  assert!(inline.environment.secret.is_empty());
  assert!(inline.deliverables.is_empty());
  assert!(!inline.requires_tool_authorization());

  let complete = json!({
    "prompt_file": "prompts/review.md",
    "model": "gpt-codex",
    "reasoning_effort": "high",
    "result_schema": {
      "type": "object",
      "properties": { "summary": { "type": "string" } },
      "required": ["summary"],
      "additionalProperties": false
    },
    "run_records": ".octa/custom-codex-runs",
    "environment": {
      "public": { "SOURCE_REVISION": "revision" },
      "secret": { "OPENAI_API_KEY": "openai_key" }
    },
    "source_revision": "8de7f1c",
    "tool_authorization": "required",
    "deliverables": [
      {
        "kind": "artifact",
        "name": "patch",
        "path": "out/changes.patch",
        "content_type": "text/x-diff"
      },
      {
        "kind": "report",
        "name": "review",
        "path": "out/review.json",
        "format": "codex.review.v1"
      }
    ]
  });
  assert!(validator().is_valid(&complete));
  let complete = CodexConfig::parse(complete).unwrap();
  assert_eq!(complete.prompt_file.as_deref(), Some("prompts/review.md"));
  assert_eq!(complete.reasoning_effort, Some(ReasoningEffort::High));
  assert_eq!(complete.deliverables.len(), 2);
  assert!(complete.requires_tool_authorization());
}

#[test]
fn runtime_cli_settings_cannot_replace_security_model_output_or_hook_controls() {
  for argument in super::CLI_ARGUMENTS {
    let value = json!({"prompt":"work", "cli_arguments":[argument]});
    assert!(validator().is_valid(&value));
    CodexConfig::parse(value).unwrap();
  }
  for arguments in [
    vec!["--sandbox=danger-full-access"],
    vec!["--ask-for-approval=on-request"],
    vec!["--config=hooks.PreToolUse=[]"],
    vec!["--model=other"],
    vec!["--json=false"],
    vec!["--output-schema=other"],
    vec!["--"],
    vec!["resume"],
    vec!["--ephemeral", "--ephemeral"],
  ] {
    assert_schema_and_parser_reject(json!({"prompt":"work", "cli_arguments":arguments}));
  }
  assert!(CodexConfig::parse(json!({"prompt":"work", "cli_arguments":["--color=never","--color=always"]})).is_err());
}

#[test]
fn mcp_credentials_require_explicit_secret_mappings_and_encrypted_transport() {
  let valid = json!({"prompt":"work", "environment":{"secret":{"TOKEN":"credential"}},
    "mcp_servers":{"tickets":{"transport":"http", "url":"https://tickets.example/mcp", "bearer_token_env_var":"TOKEN"}}});
  assert!(validator().is_valid(&valid));
  CodexConfig::parse(valid.clone()).unwrap();
  let mut plaintext = valid.clone();
  plaintext["mcp_servers"]["tickets"]["url"] = json!("http://tickets.example/mcp");
  assert!(
    CodexConfig::parse(plaintext).is_err(),
    "authentication must not travel over plaintext HTTP"
  );
  let mut missing = valid.clone();
  missing["environment"] = json!({});
  assert!(CodexConfig::parse(missing).is_err());
  let mut public = valid;
  public["environment"] = json!({"public":{"TOKEN":"credential"}});
  assert!(CodexConfig::parse(public).is_err());
}

#[test]
fn mcp_rejects_unknown_settings_unbounded_content_and_ambient_environment() {
  let value = |server: Value| json!({"prompt":"work", "mcp_servers":{"selected":server}});
  for server in [
    json!({"transport":"http", "url":"https://tickets.example/mcp", "http_headers":{"Authorization":"inline-secret"}}),
    json!({"transport":"other", "url":"https://tickets.example/mcp"}),
    json!({"transport":"stdio", "command":"/opt/mcp", "env_vars":["TOKEN","TOKEN"]}),
    json!({"transport":"stdio", "command":"/opt/mcp", "args":["\u{0000}"]}),
    json!({"transport":"http", "url":"https://tickets.example/mcp", "enabled_tools":["read","read"]}),
  ] {
    assert_schema_and_parser_reject(value(server));
  }
  for server in [
    json!({"transport":"http", "url":"file:///private/token"}),
    json!({"transport":"http", "url":"not-a-url"}),
    json!({"transport":"http", "url":"https://user:password@tickets.example/mcp"}),
    json!({"transport":"http", "url":"https://tickets.example/mcp#fragment"}),
    json!({"transport":"stdio", "command":"ambient-mcp"}),
    json!({"transport":"stdio", "command":std::env::current_exe().unwrap(), "env_vars":["UNMAPPED_SECRET"]}),
  ] {
    assert!(CodexConfig::parse(value(server)).is_err());
  }
  assert_schema_and_parser_reject(
    json!({"prompt":"work", "mcp_servers":{"invalid.name":{"transport":"http","url":"https://example.com"}}}),
  );
  let oversized = (0..65)
    .map(|i| {
      (
        format!("server-{i}"),
        json!({"transport":"http","url":"https://example.com"}),
      )
    })
    .collect::<serde_json::Map<_, _>>();
  assert_schema_and_parser_reject(json!({"prompt":"work", "mcp_servers":oversized}));
  let large = (0..64)
    .map(|i| {
      (
        format!("server-{i}"),
        json!({"transport":"http","url":format!("https://example.com/{}", "x".repeat(2048))}),
      )
    })
    .collect::<serde_json::Map<_, _>>();
  assert!(CodexConfig::parse(json!({"prompt":"work", "mcp_servers":large})).is_err());
}

#[test]
fn tool_authorization_is_explicit_and_has_no_best_effort_mode() {
  let required = json!({ "prompt": "work", "tool_authorization": "required" });
  assert!(validator().is_valid(&required));
  assert!(CodexConfig::parse(required).unwrap().requires_tool_authorization());

  for value in [
    json!({ "prompt": "work", "tool_authorization": "optional" }),
    json!({ "prompt": "work", "tool_authorization": true }),
    json!({ "prompt": "work", "tool_authorization": {} }),
  ] {
    assert_schema_and_parser_reject(value);
  }
}

#[test]
fn rejects_unknown_fields_and_ambiguous_prompt_sources() {
  for value in [
    json!({}),
    json!({ "prompt": "one", "prompt_file": "prompt.md" }),
    json!({ "prompt": "one", "unknown": true }),
    json!({ "prompt": "one", "environment": { "unknown": {} } }),
    json!({
      "prompt": "one",
      "deliverables": [{ "kind": "artifact", "name": "a", "path": "a", "unknown": true }]
    }),
  ] {
    assert_schema_and_parser_reject(value);
  }
}

#[test]
fn every_path_field_rejects_each_nonportable_or_unsafe_form() {
  let invalid_paths = [
    "",
    ".",
    "..",
    "/absolute",
    "C:/absolute",
    r"nested\file",
    "nested/../escape",
    "./nested",
    "nested/./file",
    "nested//file",
    "nested/file/",
    "nested\nfile",
    "nested\u{7f}file",
    "nested/file:stream",
    "nested/file*glob",
    "nested/question?mark",
    "nested/quoted\"name",
    "nested/pipe|name",
    "nested/less<than",
    "nested/greater>than",
    "nested/trailing.",
    "nested/trailing ",
    "CON",
    "nested/nul.txt",
    "nested/Com1.log",
    "LPT9",
  ];

  for path in invalid_paths {
    for value in [
      json!({ "prompt_file": path }),
      json!({ "prompt": "work", "run_records": path }),
      json!({
        "prompt": "work",
        "deliverables": [{ "kind": "artifact", "name": "output", "path": path }]
      }),
      json!({
        "prompt": "work",
        "deliverables": [{ "kind": "report", "name": "report", "path": path, "format": "json" }]
      }),
    ] {
      assert_schema_and_parser_reject(value);
    }
  }
}

#[test]
fn accepts_names_that_only_resemble_windows_devices() {
  for path in ["conifer", "aux-data", "com10", "lpt0", "éa1", "nested/file.name"] {
    let value = json!({ "prompt_file": path });
    assert!(validator().is_valid(&value), "schema rejected {path}");
    assert!(CodexConfig::parse(value).is_ok(), "parser rejected {path}");
  }
}

#[test]
fn run_record_roots_reserve_space_for_generated_paths() {
  let largest_root = "r".repeat(MAX_RUN_RECORDS_BYTES);
  let valid = json!({ "prompt": "work", "run_records": largest_root });
  assert!(validator().is_valid(&valid));
  assert!(CodexConfig::parse(valid).is_ok());

  assert_schema_and_parser_reject(json!({
    "prompt": "work",
    "run_records": "r".repeat(MAX_RUN_RECORDS_BYTES + 1)
  }));

  let full_size_non_record_path = "p".repeat(MAX_PATH_BYTES);
  let prompt = json!({ "prompt_file": full_size_non_record_path });
  assert!(validator().is_valid(&prompt));
  assert!(CodexConfig::parse(prompt).is_ok());
}

#[test]
fn rejects_invalid_environment_mappings_and_duplicate_deliverables() {
  let public = (0..33)
    .map(|index| (format!("PUBLIC_{index}"), json!(format!("public_{index}"))))
    .collect::<serde_json::Map<_, _>>();
  let secret = (0..32)
    .map(|index| (format!("SECRET_{index}"), json!(format!("secret_{index}"))))
    .collect::<serde_json::Map<_, _>>();

  for value in [
    json!({ "prompt": "work", "environment": { "public": { "9INVALID": "value" } } }),
    json!({ "prompt": "work", "environment": { "public": { "VALID": " value" } } }),
    json!({
      "prompt": "work",
      "environment": {
        "public": { "TOKEN": "public_token" },
        "secret": { "TOKEN": "secret_token" }
      }
    }),
    json!({
      "prompt": "work",
      "environment": {
        "public": { "token": "public_token" },
        "secret": { "TOKEN": "secret_token" }
      }
    }),
    json!({ "prompt": "work", "environment": { "public": public, "secret": secret } }),
    json!({
      "prompt": "work",
      "deliverables": [
        { "kind": "artifact", "name": "result", "path": "out/result" },
        { "kind": "report", "name": "result", "path": "out/result.json", "format": "json" }
      ]
    }),
  ] {
    assert!(CodexConfig::parse(value).is_err());
  }
}

#[test]
fn rejects_deliverables_that_shadow_plugin_owned_run_records() {
  for name in RESERVED_RESOURCE_NAMES {
    let value = json!({
      "prompt": "work",
      "deliverables": [{ "kind": "artifact", "name": name, "path": "out/value" }]
    });
    assert!(
      validator().is_valid(&value),
      "wire schema unexpectedly owns semantic names"
    );
    let error = CodexConfig::parse(value).unwrap_err();
    assert!(error.to_string().contains("reserved for a Codex run record"));
  }
}

#[test]
fn accepts_only_the_documented_reasoning_efforts() {
  for effort in ["none", "minimal", "low", "medium", "high", "xhigh", "max"] {
    let value = json!({ "prompt": "work", "reasoning_effort": effort });
    assert!(validator().is_valid(&value), "schema rejected {effort}");
    assert!(CodexConfig::parse(value).is_ok(), "parser rejected {effort}");
  }

  assert_schema_and_parser_reject(json!({ "prompt": "work", "reasoning_effort": "extreme" }));
}

#[test]
fn rejects_blank_or_malformed_text_and_deliverable_metadata() {
  for value in [
    json!({ "prompt": " \n\t " }),
    json!({ "prompt": "work\0hidden" }),
    json!({ "prompt": "work", "model": " model" }),
    json!({ "prompt": "work", "source_revision": "revision\nnext" }),
    json!({
      "prompt": "work",
      "deliverables": [{ "kind": "artifact", "name": "", "path": "out/file" }]
    }),
    json!({
      "prompt": "work",
      "deliverables": [{
        "kind": "artifact",
        "name": "output",
        "path": "out/file",
        "content_type": ""
      }]
    }),
    json!({
      "prompt": "work",
      "deliverables": [{ "kind": "report", "name": "report", "path": "out/report", "format": "" }]
    }),
  ] {
    assert_schema_and_parser_reject(value);
  }
}

#[test]
fn enforces_configuration_collection_and_string_bounds() {
  let too_many_mappings = (0..=MAX_ENVIRONMENT_MAPPINGS)
    .map(|index| (format!("VALUE_{index}"), json!(format!("var_{index}"))))
    .collect::<serde_json::Map<_, _>>();
  let too_many_deliverables = (0..=MAX_DELIVERABLES)
    .map(|index| json!({ "kind": "artifact", "name": format!("item-{index}"), "path": format!("out/{index}") }))
    .collect::<Vec<_>>();

  for value in [
    json!({ "prompt": "x".repeat(MAX_PROMPT_BYTES + 1) }),
    json!({ "prompt": "work", "model": "x".repeat(MAX_NAME_BYTES + 1) }),
    json!({ "prompt": "work", "source_revision": "x".repeat(MAX_SOURCE_REVISION_BYTES + 1) }),
    json!({ "prompt": "work", "environment": { "public": too_many_mappings } }),
    json!({ "prompt": "work", "deliverables": too_many_deliverables }),
  ] {
    assert_schema_and_parser_reject(value);
  }
}

#[test]
fn rejects_invalid_or_oversized_result_schemas() {
  let invalid = json!({ "prompt": "work", "result_schema": { "type": 7 } });
  assert!(validator().is_valid(&invalid));
  assert!(CodexConfig::parse(invalid).is_err());

  let oversized = json!({
    "prompt": "work",
    "result_schema": {
      "type": "string",
      "description": "x".repeat(MAX_RESULT_SCHEMA_BYTES)
    }
  });
  assert!(validator().is_valid(&oversized));
  assert!(CodexConfig::parse(oversized).is_err());
}
