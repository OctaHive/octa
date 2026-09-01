use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// A validated variable definition with its value source, requirement, and logging sensitivity.
#[derive(Clone, Eq, PartialEq)]
pub struct Variable {
  value: VariableValue,
  secret: bool,
}

impl fmt::Debug for Variable {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    // A derived `Debug` would expose both literal secrets and commands used to obtain them.
    let value = if self.secret {
      &"*****" as &dyn fmt::Debug
    } else {
      &self.value as &dyn fmt::Debug
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
enum VariableValue {
  Value(Value),
  Shell(String),
  Required,
}

impl Variable {
  /// Returns whether diagnostics must redact the resolved variable value.
  pub fn is_secret(&self) -> bool {
    self.secret
  }

  /// Returns whether a higher-priority variable layer must provide the value.
  pub fn is_required(&self) -> bool {
    matches!(self.value, VariableValue::Required)
  }

  /// Returns the value representation consumed by the executor.
  pub fn into_value(self) -> Option<Value> {
    match self.value {
      VariableValue::Value(value) => Some(value),
      VariableValue::Shell(command) => Some(serde_json::json!({ "sh": command })),
      VariableValue::Required => None,
    }
  }

  /// Returns the unresolved value used while rendering include path templates.
  pub(crate) fn template_value(&self) -> Option<Value> {
    match &self.value {
      VariableValue::Value(value) => Some(value.clone()),
      VariableValue::Shell(command) => Some(serde_json::json!({ "sh": command })),
      VariableValue::Required => None,
    }
  }

  /// Parses shorthand values and explicit variable definitions.
  fn from_json(value: Value) -> Result<Self, String> {
    let Value::Object(mut definition) = value else {
      return Ok(Self {
        value: VariableValue::Value(value),
        secret: false,
      });
    };

    if !definition.contains_key("secret") && !definition.contains_key("required") {
      // An exact `{ sh: ... }` mapping is executable; every other mapping remains user data.
      if definition.len() == 1 {
        if let Some(Value::String(command)) = definition.get("sh") {
          return Ok(Self {
            value: VariableValue::Shell(command.clone()),
            secret: false,
          });
        }
      }
      return Ok(Self {
        value: VariableValue::Value(Value::Object(definition)),
        secret: false,
      });
    }

    // Metadata keys turn the mapping into a variable definition rather than an arbitrary object value.
    if let Some(name) = definition
      .keys()
      .find(|name| !matches!(name.as_str(), "value" | "sh" | "secret" | "required"))
    {
      return Err(format!("unknown variable option '{name}'"));
    }

    let secret = definition
      .remove("secret")
      .map(|value| value.as_bool().ok_or_else(|| "'secret' must be a boolean".to_owned()))
      .transpose()?
      .unwrap_or(false);
    let required = definition
      .remove("required")
      .map(|value| value.as_bool().ok_or_else(|| "'required' must be a boolean".to_owned()))
      .transpose()?
      .unwrap_or(false);

    if required {
      if definition.contains_key("value") || definition.contains_key("sh") {
        return Err("a required variable cannot define 'value' or 'sh'".to_owned());
      }
      return Ok(Self {
        value: VariableValue::Required,
        secret,
      });
    }

    let value = match (definition.remove("value"), definition.remove("sh")) {
      (Some(value), None) => VariableValue::Value(value),
      (None, Some(Value::String(command))) => VariableValue::Shell(command),
      (None, Some(_)) => return Err("'sh' must be a string".to_owned()),
      _ => return Err("a variable definition must contain exactly one of 'value' or 'sh'".to_owned()),
    };

    Ok(Self { value, secret })
  }

  /// Reconstructs the YAML-compatible representation used by Serde.
  fn configuration_value(&self) -> Value {
    match (&self.value, self.secret) {
      (VariableValue::Value(value), false) => value.clone(),
      (VariableValue::Shell(command), false) => serde_json::json!({ "sh": command }),
      (VariableValue::Value(value), true) => serde_json::json!({ "value": value, "secret": true }),
      (VariableValue::Shell(command), true) => serde_json::json!({ "sh": command, "secret": true }),
      (VariableValue::Required, false) => serde_json::json!({ "required": true }),
      (VariableValue::Required, true) => serde_json::json!({ "required": true, "secret": true }),
    }
  }
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
