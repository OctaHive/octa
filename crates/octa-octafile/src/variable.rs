use std::{collections::HashSet, fmt};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A validated variable definition with its value source, requirement, and logging sensitivity.
#[derive(Clone, Eq, PartialEq)]
pub struct Variable {
  source: VariableSource,
  secret: bool,
  enum_source: Option<VariableEnum>,
  question: Option<String>,
}

impl fmt::Debug for Variable {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    // A derived `Debug` would expose both literal secrets and commands used to obtain them.
    let value = if self.secret {
      &"*****" as &dyn fmt::Debug
    } else {
      &self.source as &dyn fmt::Debug
    };

    formatter
      .debug_struct("Variable")
      .field("value", value)
      .field("secret", &self.secret)
      .finish()
  }
}

/// Keeps the source form until the executor is ready to resolve the variable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VariableSource {
  Value(Value),
  Shell(String),
  Secret(SecretRef),
  Required(RequiredMode),
  TaskOutput(TaskOutputReference),
}

/// Logical secret lookup resolved through an environment-specific provider profile.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretRef {
  pub provider: String,
  pub key: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub field: Option<String>,
}

/// Structured output exported by a direct task dependency.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskOutputReference {
  /// Direct dependency name as written in `deps`.
  pub task: String,
  /// Public output name declared by that dependency.
  pub output: String,
}

/// Controls how an absent required variable is handled before task execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredMode {
  Strict,
  Prompt,
}

/// Describes either literal choices or a variable reference resolved by the executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VariableEnum {
  Values(Vec<String>),
  Template(String),
}

impl Variable {
  /// Returns whether diagnostics must redact the resolved variable value.
  pub fn is_secret(&self) -> bool {
    self.secret
  }

  /// Returns whether a higher-priority variable layer must provide the value.
  pub fn is_required(&self) -> bool {
    matches!(self.source, VariableSource::Required(_))
  }

  /// Returns whether the value must be resolved from a task dependency.
  pub(crate) fn is_task_output(&self) -> bool {
    matches!(self.source, VariableSource::TaskOutput(_))
  }

  /// Returns the missing-value behavior for a required variable.
  pub fn required_mode(&self) -> Option<RequiredMode> {
    match self.source {
      VariableSource::Required(mode) => Some(mode),
      _ => None,
    }
  }

  /// Returns choices declared as a literal list rather than a template.
  pub fn enum_values(&self) -> Option<&[String]> {
    match &self.enum_source {
      Some(VariableEnum::Values(values)) => Some(values),
      _ => None,
    }
  }

  /// Returns the enum definition resolved before required input is requested.
  pub fn enum_source(&self) -> Option<&VariableEnum> {
    self.enum_source.as_ref()
  }

  /// Returns the custom interactive question, when configured.
  pub fn question(&self) -> Option<&str> {
    self.question.as_deref()
  }

  /// Returns the value representation consumed by the executor.
  pub fn into_value(self) -> Option<Value> {
    match self.source {
      VariableSource::Value(value) => Some(value),
      VariableSource::Shell(command) => Some(serde_json::json!({ "sh": command })),
      VariableSource::Secret(_) => None,
      VariableSource::Required(_) => None,
      VariableSource::TaskOutput(_) => None,
    }
  }

  /// Returns the typed source consumed by the executor.
  pub fn into_source(self) -> VariableSource {
    self.source
  }

  /// Returns the unresolved value used while rendering include path templates.
  pub(crate) fn template_value(&self) -> Option<Value> {
    match &self.source {
      VariableSource::Value(value) => Some(value.clone()),
      VariableSource::Shell(command) => Some(serde_json::json!({ "sh": command })),
      VariableSource::Secret(_) => None,
      VariableSource::Required(_) => None,
      VariableSource::TaskOutput(_) => None,
    }
  }

  /// Parses shorthand values and explicit variable definitions.
  fn from_json(value: Value) -> Result<Self, String> {
    let Value::Object(mut definition) = value else {
      return Ok(Self {
        source: VariableSource::Value(value),
        secret: false,
        enum_source: None,
        question: None,
      });
    };

    if !definition
      .keys()
      .any(|name| matches!(name.as_str(), "secret" | "required" | "enum" | "question" | "from"))
    {
      // An exact `{ sh: ... }` mapping is executable; every other mapping remains user data.
      if definition.len() == 1 {
        if let Some(Value::String(command)) = definition.get("sh") {
          return Ok(Self {
            source: VariableSource::Shell(command.clone()),
            secret: false,
            enum_source: None,
            question: None,
          });
        }
      }
      return Ok(Self {
        source: VariableSource::Value(Value::Object(definition)),
        secret: false,
        enum_source: None,
        question: None,
      });
    }

    // Metadata keys turn the mapping into a variable definition rather than an arbitrary object value.
    if let Some(name) = definition.keys().find(|name| {
      !matches!(
        name.as_str(),
        "value" | "sh" | "secret" | "required" | "enum" | "question" | "from"
      )
    }) {
      return Err(format!("unknown variable option '{name}'"));
    }

    let secret_option = definition.remove("secret");
    if let Some(Value::Object(reference)) = secret_option.as_ref() {
      if !definition.is_empty() {
        return Err("a secret reference cannot define other variable options".to_owned());
      }
      let reference = serde_json::from_value::<SecretRef>(Value::Object(reference.clone()))
        .map_err(|error| format!("invalid secret reference: {error}"))?;
      if reference.provider.trim().is_empty() || reference.key.trim().is_empty() {
        return Err("secret provider and key must not be empty".to_owned());
      }
      if reference.field.as_ref().is_some_and(|field| field.trim().is_empty()) {
        return Err("secret field must not be empty".to_owned());
      }
      return Ok(Self {
        source: VariableSource::Secret(reference),
        secret: true,
        enum_source: None,
        question: None,
      });
    }
    let secret = secret_option
      .map(|value| {
        value
          .as_bool()
          .ok_or_else(|| "'secret' must be a boolean or reference".to_owned())
      })
      .transpose()?
      .unwrap_or(false);
    let required = match definition.remove("required") {
      None | Some(Value::Bool(false)) => None,
      Some(Value::Bool(true)) => Some(RequiredMode::Strict),
      Some(Value::String(value)) if value == "prompt" => Some(RequiredMode::Prompt),
      Some(_) => return Err("'required' must be a boolean or 'prompt'".to_owned()),
    };
    let enum_source = definition.remove("enum").map(parse_enum_source).transpose()?;
    let question = definition
      .remove("question")
      .map(|value| match value {
        Value::String(question) if !question.trim().is_empty() => Ok(question),
        Value::String(_) => Err("'question' must not be empty".to_owned()),
        _ => Err("'question' must be a string".to_owned()),
      })
      .transpose()?;

    if let Some(from) = definition.remove("from") {
      if required.is_some() || enum_source.is_some() || question.is_some() || !definition.is_empty() {
        return Err("'from' can only be combined with 'secret'".to_owned());
      }
      let reference = serde_json::from_value::<TaskOutputReference>(from)
        .map_err(|error| format!("invalid task output reference: {error}"))?;
      if reference.task.trim().is_empty() || reference.output.trim().is_empty() {
        return Err("task output reference names must not be empty".to_owned());
      }
      return Ok(Self {
        source: VariableSource::TaskOutput(reference),
        secret,
        enum_source: None,
        question: None,
      });
    }

    if let Some(mode) = required {
      if definition.contains_key("value") || definition.contains_key("sh") {
        return Err("a required variable cannot define 'value' or 'sh'".to_owned());
      }
      if question.is_some() && mode != RequiredMode::Prompt {
        return Err("'question' requires 'required: prompt'".to_owned());
      }
      return Ok(Self {
        source: VariableSource::Required(mode),
        secret,
        enum_source,
        question,
      });
    }

    if enum_source.is_some() || question.is_some() {
      return Err("'enum' and 'question' require a required variable".to_owned());
    }

    let source = match (definition.remove("value"), definition.remove("sh")) {
      (Some(value), None) => VariableSource::Value(value),
      (None, Some(Value::String(command))) => VariableSource::Shell(command),
      (None, Some(_)) => return Err("'sh' must be a string".to_owned()),
      _ => return Err("a variable definition must contain exactly one of 'value' or 'sh'".to_owned()),
    };

    Ok(Self {
      source,
      secret,
      enum_source: None,
      question: None,
    })
  }

  /// Reconstructs the YAML-compatible representation used by Serde.
  fn configuration_value(&self) -> Value {
    match (&self.source, self.secret) {
      (VariableSource::Value(value), false) => value.clone(),
      (VariableSource::Shell(command), false) => serde_json::json!({ "sh": command }),
      (VariableSource::Value(value), true) => serde_json::json!({ "value": value, "secret": true }),
      (VariableSource::Shell(command), true) => serde_json::json!({ "sh": command, "secret": true }),
      (VariableSource::Secret(reference), _) => serde_json::json!({ "secret": reference }),
      (VariableSource::Required(mode), secret) => {
        let required = match mode {
          RequiredMode::Strict => Value::Bool(true),
          RequiredMode::Prompt => Value::String("prompt".to_owned()),
        };
        let mut definition = serde_json::Map::from_iter([("required".to_owned(), required)]);
        if secret {
          definition.insert("secret".to_owned(), Value::Bool(true));
        }
        if let Some(enum_source) = &self.enum_source {
          let value = match enum_source {
            VariableEnum::Values(values) => serde_json::json!(values),
            VariableEnum::Template(template) => Value::String(template.clone()),
          };
          definition.insert("enum".to_owned(), value);
        }
        if let Some(question) = &self.question {
          definition.insert("question".to_owned(), Value::String(question.clone()));
        }
        Value::Object(definition)
      },
      (VariableSource::TaskOutput(reference), false) => serde_json::json!({ "from": reference }),
      (VariableSource::TaskOutput(reference), true) => serde_json::json!({ "from": reference, "secret": true }),
    }
  }
}

fn parse_enum_source(value: Value) -> Result<VariableEnum, String> {
  let values = match value {
    Value::Array(values) => values,
    Value::String(template) if template.contains("{{") && template.contains("}}") => {
      return Ok(VariableEnum::Template(template));
    },
    _ => return Err("'enum' must be a list of strings or a template expression".to_owned()),
  };
  if values.is_empty() {
    return Err("'enum' must not be empty".to_owned());
  }

  let mut unique = HashSet::new();
  let mut result = Vec::with_capacity(values.len());
  for value in values {
    let Value::String(value) = value else {
      return Err("'enum' must contain only strings".to_owned());
    };
    if value.trim().is_empty() {
      return Err("'enum' values must not be empty".to_owned());
    }
    if !unique.insert(value.clone()) {
      return Err(format!("duplicated enum value '{value}'"));
    }
    result.push(value);
  }

  Ok(VariableEnum::Values(result))
}

impl Serialize for Variable {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    self.configuration_value().serialize(serializer)
  }
}

impl<'de> Deserialize<'de> for Variable {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = Value::deserialize(deserializer)?;
    Self::from_json(value).map_err(serde::de::Error::custom)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn preserves_plain_object_variables() {
    let variable = Variable::from_json(json!({ "nested": true })).unwrap();

    assert!(!variable.is_required());
    assert_eq!(variable.required_mode(), None);
    assert_eq!(variable.template_value(), Some(json!({ "nested": true })));
    assert_eq!(variable.configuration_value(), json!({ "nested": true }));
    assert!(format!("{variable:?}").contains("nested"));
  }

  #[test]
  fn preserves_non_secret_shell_variables() {
    let variable = Variable::from_json(json!({ "sh": "git rev-parse HEAD" })).unwrap();

    assert_eq!(variable.template_value(), Some(json!({ "sh": "git rev-parse HEAD" })));
    assert_eq!(variable.configuration_value(), json!({ "sh": "git rev-parse HEAD" }));
  }

  #[test]
  fn required_variables_have_no_template_value() {
    let variable = Variable::from_json(json!({ "required": true })).unwrap();

    assert_eq!(variable.template_value(), None);
  }

  #[test]
  fn parses_and_serializes_task_output_references() {
    let value = json!({ "from": { "task": "image", "output": "digest" } });
    let variable = Variable::from_json(value.clone()).unwrap();

    assert!(matches!(
      variable.clone().into_source(),
      VariableSource::TaskOutput(TaskOutputReference { task, output })
        if task == "image" && output == "digest"
    ));
    assert_eq!(variable.template_value(), None);
    assert_eq!(variable.configuration_value(), value);
    assert!(Variable::from_json(json!({ "from": { "task": "", "output": "digest" } })).is_err());
    let secret = json!({
      "from": { "task": "image", "output": "digest" },
      "secret": true
    });
    let secret_variable = Variable::from_json(secret.clone()).unwrap();
    assert!(secret_variable.is_secret());
    assert_eq!(secret_variable.configuration_value(), secret);
  }

  #[test]
  fn preserves_template_backed_enums() {
    let variable = Variable::from_json(json!({
      "required": "prompt",
      "enum": "{{ ENVIRONMENTS }}"
    }))
    .unwrap();

    assert_eq!(variable.enum_values(), None);
    assert_eq!(
      variable.enum_source(),
      Some(&VariableEnum::Template("{{ ENVIRONMENTS }}".to_owned()))
    );
    assert_eq!(
      variable.configuration_value(),
      json!({ "required": "prompt", "enum": "{{ ENVIRONMENTS }}" })
    );
  }

  #[test]
  fn parses_and_serializes_logical_secret_references() {
    let value = json!({
      "secret": {
        "provider": "application",
        "key": "service/token",
        "field": "token"
      }
    });
    let variable = Variable::from_json(value.clone()).unwrap();

    assert!(variable.is_secret());
    assert_eq!(variable.template_value(), None);
    assert!(matches!(
      variable.clone().into_source(),
      VariableSource::Secret(SecretRef { provider, key, field })
        if provider == "application" && key == "service/token" && field.as_deref() == Some("token")
    ));
    assert_eq!(variable.configuration_value(), value);
    assert!(!format!("{variable:?}").contains("service/token"));
  }

  #[test]
  fn rejects_invalid_secret_references() {
    assert!(Variable::from_json(json!({ "secret": { "provider": "", "key": "token" } })).is_err());
    assert!(Variable::from_json(json!({ "secret": { "provider": "app", "key": "" } })).is_err());
    assert!(Variable::from_json(json!({
      "secret": { "provider": "app", "key": "token" },
      "value": "not-allowed"
    }))
    .is_err());
  }

  #[test]
  fn rejects_non_string_questions_and_non_list_enums() {
    assert!(Variable::from_json(json!({ "required": "prompt", "question": 1 })).is_err());
    assert!(Variable::from_json(json!({ "required": "prompt", "enum": "production" })).is_err());
  }

  #[test]
  fn distinguishes_object_values_from_invalid_explicit_definitions() {
    let object = Variable::from_json(json!({ "sh": 1 })).unwrap();
    assert_eq!(object.into_value(), Some(json!({ "sh": 1 })));

    assert!(Variable::from_json(json!({ "sh": 1, "secret": true })).is_err());
  }
}
