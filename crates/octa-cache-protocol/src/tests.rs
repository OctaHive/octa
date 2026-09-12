//! Golden compatibility and semantic identity tests.

use std::time::Duration;

use serde_json::json;

use super::*;

fn digest(byte: u8, size: u64) -> Digest {
  Digest::new(DigestAlgorithm::Blake3, [byte; 32], size)
}

fn sha256(byte: u8, size: u64) -> Digest {
  Digest::new(DigestAlgorithm::Sha256, [byte; 32], size)
}

fn descriptor() -> ActionDescriptorV1 {
  serde_json::from_str(include_str!("../fixtures/action-descriptor-v1.json")).unwrap()
}

fn action_result() -> ActionResultV1 {
  serde_json::from_str(include_str!("../fixtures/action-result-v1.json")).unwrap()
}

fn action_result_without_files() -> ActionResultV1 {
  serde_json::from_str(include_str!("../fixtures/action-result-no-files-v1.json")).unwrap()
}

#[test]
fn digest_wire_form_is_tagged_bounded_and_lowercase() {
  let value = digest(0xab, 17);
  assert_eq!(value.hex(), "ab".repeat(32));
  assert_eq!(value.to_string(), format!("blake3:{}:17", "ab".repeat(32)));
  assert_eq!(
    serde_json::from_value::<Digest>(serde_json::to_value(value).unwrap()).unwrap(),
    value
  );

  for invalid in ["ab", &"AB".repeat(32), &"gg".repeat(32)] {
    assert!(Digest::from_hex(DigestAlgorithm::Blake3, invalid, 0).is_err());
  }
  let unknown = json!({ "algorithm": "md5", "hash": "00".repeat(32), "size_bytes": 0 });
  assert!(serde_json::from_value::<Digest>(unknown).is_err());

  assert_eq!("sha256".parse::<DigestAlgorithm>().unwrap(), DigestAlgorithm::Sha256);
  assert_eq!(
    Digest::blake3(b"octa"),
    Digest::from_hex(DigestAlgorithm::Blake3, &blake3::hash(b"octa").to_hex(), 4).unwrap()
  );
}

#[test]
fn relative_paths_have_one_cross_platform_wire_meaning() {
  for valid in [".", "src", "crates/octa-cache/src"] {
    assert_eq!(RelativePath::new(valid).unwrap().as_str(), valid);
  }
  for invalid in [
    "",
    "/tmp/output",
    "C:/output",
    "src\\main.rs",
    "a//b",
    "a/./b",
    "a/../b",
    "a\nb",
  ] {
    assert!(RelativePath::new(invalid).is_err(), "accepted {invalid:?}");
  }

  let root = RelativePath::root();
  assert!(root.is_root());
  let output = RelativePath::new("target").unwrap();
  assert!(output.is_within(&root));
  assert!(RelativePath::new("target/release/app").unwrap().is_within(&output));
  assert!(output.is_within(&output));
  assert!(!RelativePath::new("target-other/app").unwrap().is_within(&output));
  assert_eq!(root.to_string(), ".");
  assert_eq!(
    "src/main.rs".parse::<RelativePath>().unwrap().to_string(),
    "src/main.rs"
  );
  assert_eq!(
    serde_json::from_str::<RelativePath>(&serde_json::to_string(&root).unwrap()).unwrap(),
    root
  );
  assert!(RelativePath::new("x".repeat(MAX_CACHE_STRING_BYTES + 1)).is_err());
}

#[test]
fn golden_action_descriptor_has_a_stable_digest() {
  let descriptor = descriptor();
  descriptor.validate().unwrap();
  let action = descriptor.digest().unwrap();
  assert_eq!(action.algorithm(), DigestAlgorithm::Blake3);
  assert_eq!(action.size_bytes(), 400);
  assert_eq!(
    action.hex(),
    "c3af817e1da00134916bc82d382553fd7c0b4d9347b4a9d381c1ce3e32051a74"
  );
}

#[test]
fn every_action_component_changes_the_identity() {
  let original = descriptor();
  let expected = original.digest().unwrap();
  let assert_changed = |candidate: ActionDescriptorV1| assert_ne!(candidate.digest().unwrap(), expected);

  let mut candidate = original.clone();
  candidate.task_definition = digest(20, 101);
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.input_root = digest(20, 202);
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.working_directory = RelativePath::new("crates/octa-runtime").unwrap();
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.variables = digest(20, 303);
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.environment = digest(20, 404);
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.runtime = RuntimeIdentity::Native {
    os: PlatformOs::Linux,
    architecture: PlatformArchitecture::Amd64,
    environment: digest(21, 21),
  };
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.runtime = RuntimeIdentity::Oci {
    os: PlatformOs::Windows,
    architecture: PlatformArchitecture::Arm64,
    image: sha256(31, 32),
  };
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.plugins[0].name = "shell-renamed".to_owned();
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.plugins[0].version = "0.4.0".to_owned();
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.plugins[0].protocol_version += 1;
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.plugins[0].executable = sha256(33, 34);
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.arguments.reverse();
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.timeout = Some(Duration::from_secs(121));
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.timeout = Some(Duration::new(120, 1));
  assert_changed(candidate);
  let mut candidate = original.clone();
  candidate.timeout = None;
  assert_changed(candidate);
  let mut candidate = original;
  candidate.salt = Some("rust-release-v2".to_owned());
  assert_changed(candidate);
}

#[test]
fn plugin_order_and_absolute_workspace_location_do_not_change_identity() {
  let mut left = descriptor();
  left.plugins.push(PluginIdentity {
    name: "junit".to_owned(),
    version: "0.3.0".to_owned(),
    protocol_version: 1,
    executable: sha256(9, 90),
  });
  let mut right = left.clone();
  right.plugins.reverse();

  // Absolute workspace roots cannot enter ActionDescriptorV1; both agents
  // construct the same descriptor from normalized workspace-relative data.
  let linux_workspace = "/agent/work/project";
  let windows_workspace = r"D:\agent\work\project";
  assert_ne!(linux_workspace, windows_workspace);
  assert_eq!(left.digest().unwrap(), right.digest().unwrap());
}

#[test]
fn action_validation_rejects_ambiguous_or_untrusted_identity_parts() {
  let mut value = descriptor();
  value.key_format = 2;
  assert!(value.validate().unwrap_err().to_string().contains("key format"));

  let mut value = descriptor();
  value.plugins.push(value.plugins[0].clone());
  assert!(value.validate().unwrap_err().to_string().contains("more than once"));

  let mut value = descriptor();
  value.plugins[0].name.clear();
  assert!(value.validate().unwrap_err().to_string().contains("must not be empty"));

  let mut value = descriptor();
  value.plugins[0].version = "bad\nversion".to_owned();
  assert!(value.validate().unwrap_err().to_string().contains("control characters"));

  let mut value = descriptor();
  value.plugins[0].version = "x".repeat(MAX_CACHE_STRING_BYTES + 1);
  assert!(value.validate().unwrap_err().to_string().contains("UTF-8 bytes"));

  let mut value = descriptor();
  value.plugins[0].protocol_version = 0;
  assert!(value.validate().unwrap_err().to_string().contains("version zero"));

  let mut value = descriptor();
  value.plugins[0].executable = digest(1, 1);
  assert!(value.validate().unwrap_err().to_string().contains("SHA-256"));

  for field in ["task_definition", "input_root", "variables", "environment"] {
    let mut value = serde_json::to_value(descriptor()).unwrap();
    value[field]["algorithm"] = json!("sha256");
    let value = serde_json::from_value::<ActionDescriptorV1>(value).unwrap();
    assert!(value.validate().unwrap_err().to_string().contains("blake3"));
  }

  let mut value = descriptor();
  value.runtime = RuntimeIdentity::Native {
    os: PlatformOs::Linux,
    architecture: PlatformArchitecture::Amd64,
    environment: sha256(1, 1),
  };
  assert!(value.validate().unwrap_err().to_string().contains("blake3"));

  let mut value = descriptor();
  value.runtime = RuntimeIdentity::Oci {
    os: PlatformOs::Linux,
    architecture: PlatformArchitecture::Amd64,
    image: digest(1, 1),
  };
  assert!(value.validate().unwrap_err().to_string().contains("sha256"));

  let mut value = descriptor();
  value.timeout = Some(Duration::ZERO);
  assert!(value.validate().unwrap_err().to_string().contains("greater than zero"));

  let mut value = descriptor();
  value.arguments = vec![String::new(); MAX_CACHE_LIST_ITEMS + 1];
  assert!(value.validate().unwrap_err().to_string().contains("limited"));

  let mut value = descriptor();
  value.arguments = vec!["x".repeat(MAX_CACHE_STRING_BYTES + 1)];
  assert!(value.validate().unwrap_err().to_string().contains("argument exceeds"));

  let mut value = descriptor();
  value.salt = Some(String::new());
  assert!(value.validate().unwrap_err().to_string().contains("must not be empty"));

  let invalid_duration = serde_json::json!({
    "seconds": 1,
    "nanoseconds": 1_000_000_000_u64,
  });
  let mut wire = serde_json::to_value(descriptor()).unwrap();
  wire["timeout"] = invalid_duration;
  assert!(serde_json::from_value::<ActionDescriptorV1>(wire).is_err());

  let mut without_salt = descriptor();
  without_salt.salt = None;
  assert_ne!(without_salt.digest().unwrap(), descriptor().digest().unwrap());
}

#[test]
fn golden_documents_match_schemas_and_wire_types() {
  let descriptor_json: serde_json::Value =
    serde_json::from_str(include_str!("../fixtures/action-descriptor-v1.json")).unwrap();
  let result_json: serde_json::Value = serde_json::from_str(include_str!("../fixtures/action-result-v1.json")).unwrap();
  let result_without_files_json: serde_json::Value =
    serde_json::from_str(include_str!("../fixtures/action-result-no-files-v1.json")).unwrap();
  let descriptor_schema: serde_json::Value = serde_json::from_str(ACTION_DESCRIPTOR_SCHEMA_V1).unwrap();
  let result_schema: serde_json::Value = serde_json::from_str(ACTION_RESULT_SCHEMA_V1).unwrap();

  assert!(jsonschema::validator_for(&descriptor_schema)
    .unwrap()
    .is_valid(&descriptor_json));
  assert!(jsonschema::validator_for(&result_schema)
    .unwrap()
    .is_valid(&result_json));
  assert!(jsonschema::validator_for(&result_schema)
    .unwrap()
    .is_valid(&result_without_files_json));
  serde_json::from_value::<ActionDescriptorV1>(descriptor_json)
    .unwrap()
    .validate()
    .unwrap();
  serde_json::from_value::<ActionResultV1>(result_json)
    .unwrap()
    .validate()
    .unwrap();
  let result_without_files = serde_json::from_value::<ActionResultV1>(result_without_files_json).unwrap();
  result_without_files.validate().unwrap();
  assert!(result_without_files.output_bundle.is_none());
  assert!(serde_json::to_value(result_without_files)
    .unwrap()
    .get("output_bundle")
    .is_none());
}

#[test]
fn schemas_and_serde_reject_nonportable_or_unknown_wire_values() {
  let descriptor_schema: serde_json::Value = serde_json::from_str(ACTION_DESCRIPTOR_SCHEMA_V1).unwrap();
  let result_schema: serde_json::Value = serde_json::from_str(ACTION_RESULT_SCHEMA_V1).unwrap();
  let descriptor_validator = jsonschema::validator_for(&descriptor_schema).unwrap();
  let result_validator = jsonschema::validator_for(&result_schema).unwrap();

  let mut value = serde_json::to_value(descriptor()).unwrap();
  value["working_directory"] = json!(".");
  assert!(descriptor_validator.is_valid(&value));
  serde_json::from_value::<ActionDescriptorV1>(value)
    .unwrap()
    .validate()
    .unwrap();

  for field in ["task_definition", "input_root", "variables", "environment"] {
    let mut value = serde_json::to_value(descriptor()).unwrap();
    value[field]["algorithm"] = json!("sha256");
    assert!(!descriptor_validator.is_valid(&value));
  }

  let mut value = serde_json::to_value(descriptor()).unwrap();
  value["working_directory"] = json!("src/../outside");
  assert!(!descriptor_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionDescriptorV1>(value).is_err());

  let mut value = serde_json::to_value(descriptor()).unwrap();
  value["plugins"][0]["name"] = json!("bad\nname");
  assert!(!descriptor_validator.is_valid(&value));

  let mut value = serde_json::to_value(descriptor()).unwrap();
  value["runtime"]["image"]["algorithm"] = json!("blake3");
  assert!(!descriptor_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionDescriptorV1>(value)
    .unwrap()
    .validate()
    .is_err());

  let mut value = serde_json::to_value(descriptor()).unwrap();
  value["unexpected"] = json!(true);
  assert!(!descriptor_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionDescriptorV1>(value).is_err());

  let mut value = serde_json::to_value(action_result()).unwrap();
  value["artifacts"][0]["path"] = json!("target/../outside");
  assert!(!result_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionResultV1>(value).is_err());

  let mut value = serde_json::to_value(action_result()).unwrap();
  value["output_bundle"]["digest"]["algorithm"] = json!("sha256");
  assert!(!result_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionResultV1>(value)
    .unwrap()
    .validate()
    .is_err());

  let mut value = serde_json::to_value(action_result()).unwrap();
  value["task_outputs"] = json!({ "bad\nname": 1 });
  assert!(!result_validator.is_valid(&value));

  let mut value = serde_json::to_value(action_result()).unwrap();
  value["task_outputs"] = json!({ "missing": null });
  assert!(!result_validator.is_valid(&value));

  let mut value = serde_json::to_value(action_result()).unwrap();
  value["output_bundle"]["encoding"] = json!("future_encoding");
  assert!(!result_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionResultV1>(value).is_err());

  let mut value = serde_json::to_value(action_result()).unwrap();
  value["unexpected"] = json!(true);
  assert!(!result_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionResultV1>(value).is_err());

  let mut value = serde_json::to_value(action_result_without_files()).unwrap();
  value["artifacts"] = serde_json::to_value(&action_result().artifacts).unwrap();
  assert!(!result_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionResultV1>(value)
    .unwrap()
    .validate()
    .is_err());

  let mut value = serde_json::to_value(action_result_without_files()).unwrap();
  value["output_bundle"] = serde_json::Value::Null;
  assert!(!result_validator.is_valid(&value));
  assert!(serde_json::from_value::<ActionResultV1>(value).is_err());
}

#[test]
fn action_result_rejects_inconsistent_or_unbounded_metadata() {
  let result = action_result();
  result.validate().unwrap();

  let mut invalid = result.clone();
  invalid.result_version += 1;
  assert!(invalid.validate().unwrap_err().to_string().contains("result version"));

  let mut invalid = result.clone();
  invalid.action = sha256(1, 1);
  assert!(invalid.validate().unwrap_err().to_string().contains("action identity"));

  let mut invalid = result.clone();
  let bundle = invalid.output_bundle.as_mut().unwrap();
  bundle.digest = sha256(1, bundle.expanded_size_bytes);
  assert!(invalid.validate().unwrap_err().to_string().contains("output bundle"));

  let mut invalid = result.clone();
  invalid.output_bundle.as_mut().unwrap().expanded_size_bytes += 1;
  assert!(invalid.validate().unwrap_err().to_string().contains("expanded size"));

  for mutate in [
    |bundle: &mut BlobDescriptor| bundle.encoded_size_bytes = 0,
    |bundle: &mut BlobDescriptor| bundle.entry_count = 0,
  ] {
    let mut invalid = result.clone();
    mutate(invalid.output_bundle.as_mut().unwrap());
    assert!(invalid
      .validate()
      .unwrap_err()
      .to_string()
      .contains("greater than zero"));
  }

  let mut invalid = result.clone();
  let bundle = invalid.output_bundle.as_mut().unwrap();
  bundle.expanded_size_bytes = 0;
  bundle.digest = digest(1, 0);
  assert!(invalid
    .validate()
    .unwrap_err()
    .to_string()
    .contains("greater than zero"));

  let mut invalid = result.clone();
  invalid.output_bundle.as_mut().unwrap().encoding = BlobEncoding::Identity;
  assert!(invalid.validate().unwrap_err().to_string().contains("sizes must match"));

  let without_files = action_result_without_files();
  without_files.validate().unwrap();

  let mut invalid = without_files.clone();
  invalid.artifacts.push(result.artifacts[0].clone());
  assert!(invalid
    .validate()
    .unwrap_err()
    .to_string()
    .contains("require an output bundle"));

  let mut invalid = without_files;
  invalid.reports.push(result.reports[0].clone());
  assert!(invalid
    .validate()
    .unwrap_err()
    .to_string()
    .contains("require an output bundle"));

  let mut invalid = result.clone();
  invalid.artifacts = vec![result.artifacts[0].clone(); MAX_CACHE_LIST_ITEMS + 1];
  assert!(invalid.validate().unwrap_err().to_string().contains("limited"));

  let mut invalid = result.clone();
  invalid.stdout = Some("x".repeat(MAX_ACTION_RESULT_METADATA_BYTES + 1));
  assert!(invalid.validate().unwrap_err().to_string().contains("stdout exceeds"));

  let mut invalid = result.clone();
  invalid.task_outputs.insert(String::new(), json!(1));
  assert!(invalid
    .validate()
    .unwrap_err()
    .to_string()
    .contains("must not be empty"));

  let mut invalid = result.clone();
  invalid.artifacts[0].name = "bad\nname".to_owned();
  assert!(invalid
    .validate()
    .unwrap_err()
    .to_string()
    .contains("control characters"));

  let mut invalid = result.clone();
  invalid.artifacts[0].content_type = Some("application/test".to_owned());
  invalid.validate().unwrap();

  let mut invalid = result.clone();
  invalid.artifacts[0].content_type = None;
  invalid.validate().unwrap();

  let mut invalid = result.clone();
  invalid.artifacts[0].content_type = Some(String::new());
  assert!(invalid
    .validate()
    .unwrap_err()
    .to_string()
    .contains("must not be empty"));

  let mut invalid = result.clone();
  invalid.reports[0].name = "x".repeat(MAX_CACHE_STRING_BYTES + 1);
  assert!(invalid.validate().unwrap_err().to_string().contains("UTF-8 bytes"));

  let mut invalid = result.clone();
  invalid.artifacts[0].path = RelativePath::root();
  assert!(invalid.validate().unwrap_err().to_string().contains("workspace root"));

  let mut invalid = result.clone();
  invalid.reports[0].path = RelativePath::root();
  assert!(invalid.validate().unwrap_err().to_string().contains("workspace root"));

  let mut invalid = result;
  invalid.task_outputs.insert("empty".to_owned(), serde_json::Value::Null);
  assert!(invalid
    .validate()
    .unwrap_err()
    .to_string()
    .contains("concrete public value"));

  let mut invalid = action_result();
  invalid
    .task_outputs
    .insert("large".to_owned(), json!("x".repeat(MAX_ACTION_RESULT_METADATA_BYTES)));
  assert!(invalid
    .validate()
    .unwrap_err()
    .to_string()
    .contains("result metadata exceeds"));
}
