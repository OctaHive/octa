//! Resolution and validation of dynamic choices for required variables.

use std::collections::HashSet;

use indexmap::IndexMap;
use lazy_static::lazy_static;
use octa_octafile::VariableEnum;
use regex::Regex;
use tera::{Context, Tera, Value};

use crate::{
  error::{ExecutorError, ExecutorResult},
  template::format_tera_error,
};

lazy_static! {
  static ref TEMPLATE_REGEX: Regex = Regex::new(r"\{\{\s*[^{}]+\s*\}\}").unwrap();
  static ref VARIABLE_REFERENCE_REGEX: Regex = Regex::new(r"^\s*\{\{\s*([A-Za-z_][A-Za-z0-9_]*)\s*\}\}\s*$").unwrap();
}

pub(crate) fn resolve(
  name: &str,
  source: &VariableEnum,
  values: &IndexMap<String, Value>,
  secrets: &HashSet<String>,
) -> ExecutorResult<Vec<String>> {
  // Enum choices are displayed in the terminal, so secret values must not enter their context.
  let public_values = values
    .iter()
    .filter(|(name, _)| !secrets.contains(*name))
    .map(|(name, value)| (name.clone(), value.clone()))
    .collect::<IndexMap<_, _>>();
  let context = Context::from_serialize(&public_values)
    .map_err(|error| enum_error(name, format!("failed to build template context: {error}")))?;
  let value = source_value(name, source, &public_values, &context)?;

  validate_values(name, value, &context)
}

fn source_value(
  name: &str,
  source: &VariableEnum,
  values: &IndexMap<String, Value>,
  context: &Context,
) -> ExecutorResult<Value> {
  match source {
    VariableEnum::Values(options) => Ok(Value::Array(options.iter().cloned().map(Value::String).collect())),
    VariableEnum::Template(template) => {
      if let Some(reference) = VARIABLE_REFERENCE_REGEX
        .captures(template)
        .and_then(|captures| captures.get(1))
      {
        return values
          .get(reference.as_str())
          .cloned()
          .ok_or_else(|| enum_error(name, format!("variable '{}' is not available", reference.as_str())));
      }

      let rendered =
        Tera::one_off(template, context, false).map_err(|error| enum_error(name, format_tera_error(&error)))?;
      serde_json::from_str(rendered.trim())
        .map_err(|_| enum_error(name, "template must resolve to a list of strings".to_owned()))
    },
  }
}

fn validate_values(name: &str, value: Value, context: &Context) -> ExecutorResult<Vec<String>> {
  let Value::Array(values) = value else {
    return Err(enum_error(name, "value must be a list of strings".to_owned()));
  };
  if values.is_empty() {
    return Err(enum_error(name, "list must not be empty".to_owned()));
  }

  let mut unique = HashSet::new();
  let mut result = Vec::with_capacity(values.len());
  for value in values {
    let Value::String(value) = value else {
      return Err(enum_error(name, "list must contain only strings".to_owned()));
    };
    let value = render_option(name, &value, context)?;
    if value.trim().is_empty() {
      return Err(enum_error(name, "values must not be empty".to_owned()));
    }
    if !unique.insert(value.clone()) {
      return Err(enum_error(name, format!("duplicated value '{value}'")));
    }
    result.push(value);
  }

  Ok(result)
}

fn render_option(name: &str, option: &str, context: &Context) -> ExecutorResult<String> {
  if !TEMPLATE_REGEX.is_match(option) {
    return Ok(option.to_owned());
  }

  Tera::one_off(option, context, false).map_err(|error| enum_error(name, format_tera_error(&error)))
}

fn enum_error(name: &str, message: String) -> ExecutorError {
  ExecutorError::RequiredVariableEnumError(name.to_owned(), message)
}

#[cfg(test)]
mod tests {
  use serde_json::json;

  use super::*;

  fn values(entries: &[(&str, Value)]) -> IndexMap<String, Value> {
    entries
      .iter()
      .map(|(name, value)| ((*name).to_owned(), value.clone()))
      .collect()
  }

  #[test]
  fn resolves_a_reference_and_expands_its_options() {
    let values = values(&[
      ("PREFIX", json!("prod")),
      ("ENVIRONMENTS", json!(["development", "{{ PREFIX }}uction"])),
    ]);

    let resolved = resolve(
      "ENVIRONMENT",
      &VariableEnum::Template("{{ ENVIRONMENTS }}".to_owned()),
      &values,
      &HashSet::new(),
    )
    .unwrap();

    assert_eq!(resolved, ["development", "production"]);
  }

  #[test]
  fn resolves_tera_expressions_that_produce_json_lists() {
    let values = values(&[("ENVIRONMENTS", json!(["development", "production"]))]);

    let resolved = resolve(
      "ENVIRONMENT",
      &VariableEnum::Template("{{ ENVIRONMENTS | json_encode }}".to_owned()),
      &values,
      &HashSet::new(),
    )
    .unwrap();

    assert_eq!(resolved, ["development", "production"]);
  }

  #[test]
  fn secret_values_are_not_available_to_enums() {
    let values = values(&[("PRIVATE_ENVIRONMENTS", json!(["development", "production"]))]);
    let secrets = HashSet::from(["PRIVATE_ENVIRONMENTS".to_owned()]);

    assert!(matches!(
      resolve(
        "ENVIRONMENT",
        &VariableEnum::Template("{{ PRIVATE_ENVIRONMENTS }}".to_owned()),
        &values,
        &secrets,
      ),
      Err(ExecutorError::RequiredVariableEnumError(name, message))
        if name == "ENVIRONMENT" && message.contains("not available")
    ));
  }

  #[test]
  fn validates_dynamically_resolved_lists() {
    for (value, expected) in [
      (json!([]), "list must not be empty"),
      (json!([1]), "list must contain only strings"),
      (json!([""]), "values must not be empty"),
      (json!(["same", "same"]), "duplicated value 'same'"),
    ] {
      let values = values(&[("OPTIONS", value)]);
      assert!(matches!(
        resolve(
          "CHOICE",
          &VariableEnum::Template("{{ OPTIONS }}".to_owned()),
          &values,
          &HashSet::new(),
        ),
        Err(ExecutorError::RequiredVariableEnumError(_, message)) if message == expected
      ));
    }
  }
}
