use std::{collections::HashMap, sync::OnceLock};

use regex::Regex;
use serde_json::Value;

/// Renders output metadata with the same Tera semantics for prefixes and group markers.
pub fn render_output_template(template: &str, values: &HashMap<String, Value>) -> Result<String, tera::Error> {
  let context = tera::Context::from_serialize(values)?;
  let normalized = normalize(template);
  tera::Tera::one_off(&normalized, &context, false)
}

/// Parses an output template without requiring runtime task variables.
pub fn validate_output_template(template: &str) -> Result<(), tera::Error> {
  let mut tera = tera::Tera::default();
  tera.add_raw_template("output", &normalize(template))
}

fn normalize(template: &str) -> std::borrow::Cow<'_, str> {
  static DOTTED_VARIABLE: OnceLock<Regex> = OnceLock::new();
  let dotted = DOTTED_VARIABLE
    .get_or_init(|| Regex::new(r"\{\{\s*\.([A-Za-z_][A-Za-z0-9_]*)\s*\}\}").expect("static output template regex"));
  dotted.replace_all(template, "{{ $1 }}")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn validates_templates_without_requiring_runtime_values() {
    validate_output_template("BEGIN {{.TASK}} {{ ENVIRONMENT }}").unwrap();
    assert!(validate_output_template("{{ broken").is_err());
  }
}
