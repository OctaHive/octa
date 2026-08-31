use std::{
  collections::{HashMap, HashSet},
  env,
  fmt::{Debug, Display, Formatter},
  path::PathBuf,
  sync::Arc,
};

use indexmap::IndexMap;
use lazy_static::lazy_static;
use octa_octafile::Vars as OctafileVars;
use octa_plugin::logger::collect_value_redactions;
use regex::Regex;
use serde::Serialize;
use tera::{Context, Tera, Value};
use tracing::debug;

use crate::{
  error::{ExecutorError, ExecutorResult},
  function::{format_tera_error, register_shell_with_redactions, ExecuteShell},
};

lazy_static! {
  static ref TEMPLATE_REGEX: Regex = Regex::new(r"\{\{\s*[^{}]+\s*\}\}").unwrap();
}

#[derive(Clone, Default)]
pub struct Vars {
  // IndexMap is required because same-level references follow Octafile declaration order.
  values: IndexMap<String, Value>,
  // Sensitivity is runtime metadata and must stay separate from values passed to Tera/plugins.
  secrets: HashSet<String>,
  parent: Option<Arc<Vars>>, // Link to parent variables
  dir: Option<PathBuf>,      // Directory used by shell-backed values in this context
  expanded: bool,            // Indicates that all inherited values have been expanded
}

/// One inherited variable scope together with its execution context.
struct VariableLayer {
  values: IndexMap<String, Value>,
  secrets: HashSet<String>,
  dir: Option<PathBuf>,
}

impl PartialEq for Vars {
  fn eq(&self, other: &Self) -> bool {
    self.values == other.values && self.secrets == other.secrets
  }
}

impl Eq for Vars {}

impl Vars {
  pub fn new() -> Self {
    Self {
      values: IndexMap::new(),
      secrets: HashSet::new(),
      parent: None,
      dir: None,
      expanded: false,
    }
  }

  pub fn with_parent(parent: Vars) -> Self {
    Self {
      values: IndexMap::new(),
      secrets: HashSet::new(),
      parent: Some(Arc::new(parent)),
      dir: None,
      expanded: false,
    }
  }

  pub fn with_value<T: Serialize>(value: T) -> Self {
    let mut vars = Self::default();
    vars.set_value(value);
    vars
  }

  pub fn with_value_and_parent<T: Serialize>(value: T, parent: Vars) -> Self {
    let mut vars = Self::with_parent(parent);
    vars.set_value(value);
    vars
  }

  pub(crate) fn with_variables(value: OctafileVars) -> Self {
    let mut vars = Self::default();
    vars.set_variables(value);
    vars
  }

  pub(crate) fn with_variables_and_parent(value: OctafileVars, parent: Vars) -> Self {
    let mut vars = Self::with_parent(parent);
    vars.set_variables(value);
    vars
  }

  pub fn set_value<T: Serialize>(&mut self, value: T) {
    // Generic runtime values carry no sensitivity metadata and therefore replace secrets as public values.
    self.values = serialized_values(&value);
    self.secrets.clear();
    self.expanded = false;
  }

  pub(crate) fn set_variables(&mut self, variables: OctafileVars) {
    self.values.clear();
    self.secrets.clear();
    self.extend_variables(variables);
  }

  pub fn set_parent(&mut self, parent: Option<Vars>) {
    self.parent = parent.map(Arc::new);
    self.expanded = false;
  }

  /// Sets the working directory for `shell()` calls declared in this variable layer.
  pub fn set_dir(&mut self, dir: impl Into<PathBuf>) {
    self.dir = Some(dir.into());
    self.expanded = false;
  }

  pub fn insert<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) {
    if let Ok(value) = serde_json::to_value(value) {
      self.values.insert(key.to_owned(), value);
      self.secrets.remove(key);
    }
    self.expanded = false;
  }

  pub fn get(&self, key: &str) -> Option<&Value> {
    self.values.get(key)
  }

  pub fn extend(&mut self, source: Context) {
    // A Tera context cannot express sensitivity, so an override removes any previous secret marker.
    if let Some(values) = source.into_json().as_object() {
      for (key, value) in values {
        self.values.insert(key.clone(), value.clone());
        self.secrets.remove(key);
      }
    }
    self.expanded = false;
  }

  pub fn extend_with<T: Serialize>(&mut self, value: &T) {
    // Generic execution overrides are ordinary values unless they came through typed Octafile variables.
    for (key, value) in serialized_values(value) {
      self.values.insert(key.clone(), value);
      self.secrets.remove(&key);
    }
    self.expanded = false;
  }

  pub(crate) fn extend_variables(&mut self, variables: OctafileVars) {
    // Consume the typed parser representation directly so secret metadata is not lost in Serde.
    for (key, variable) in variables {
      let secret = variable.is_secret();
      let value = variable.into_value();
      self.values.insert(key.clone(), value);
      if secret {
        self.secrets.insert(key);
      } else {
        self.secrets.remove(&key);
      }
    }
    self.expanded = false;
  }

  pub fn iter(&self) -> impl Iterator<Item = (String, Value)> + '_ {
    self.values.iter().map(|(key, value)| (key.clone(), value.clone()))
  }

  pub async fn expand(&mut self, dry: bool) -> ExecutorResult<()> {
    let mut tera = Tera::default();

    if self.expanded {
      return Ok(());
    }

    let contexts = self.collect_context_chain();
    let (values, secrets) = self.process_context_chain(contexts, &mut tera, dry).await?;
    self.values = values;
    self.secrets = secrets;
    // Expansion flattens the hierarchy, so retaining parents would process them a second time.
    self.parent = None;
    self.expanded = true;

    Ok(())
  }

  fn collect_context_chain(&self) -> Vec<VariableLayer> {
    let mut contexts = Vec::new();
    let mut current = Some(self);

    while let Some(vars) = current {
      contexts.push(VariableLayer {
        values: vars.values.clone(),
        secrets: vars.secrets.clone(),
        dir: vars.dir.clone(),
      });
      current = vars.parent.as_ref().map(|p| p.as_ref());
    }

    contexts.into_iter().rev().collect()
  }

  async fn process_context_chain(
    &self,
    contexts: Vec<VariableLayer>,
    tera: &mut Tera,
    dry: bool,
  ) -> ExecutorResult<(IndexMap<String, Value>, HashSet<String>)> {
    let mut accumulated = IndexMap::new();
    let mut secrets = HashSet::new();

    // Each value is added immediately after expansion. This preserves YAML declaration order
    // and makes earlier values in the same `vars` mapping available to later ones.
    for layer in contexts {
      let VariableLayer {
        values,
        secrets: layer_secrets,
        dir,
      } = layer;
      let current_dir = match dir {
        Some(dir) => dir,
        None => env::current_dir()?,
      };

      for (key, value) in values {
        let secret = layer_secrets.contains(&key);
        let template_context = Context::from_serialize(&accumulated).map_err(|error| {
          ExecutorError::VariableExpandError(key.clone(), format!("failed to build template context: {error}"))
        })?;
        let environment = values_as_environment(&accumulated);
        let redactions = secret_values(&accumulated, &secrets);
        // Secret producers hide their complete command; other producers redact inherited secrets.
        let shell = register_shell_with_redactions(tera, &current_dir, environment, dry, redactions, secret);
        let processed = self
          .process_template_value(&key, &value, &template_context, tera, &shell, secret)
          .await?;

        accumulated.insert(key.clone(), processed);
        if secret {
          secrets.insert(key);
        } else {
          secrets.remove(&key);
        }
      }
    }

    Ok((accumulated, secrets))
  }

  async fn process_template_value(
    &self,
    key: &str,
    value: &Value,
    context: &Context,
    tera: &mut Tera,
    shell: &ExecuteShell,
    secret: bool,
  ) -> ExecutorResult<Value> {
    if let Some(command) = shell_command(value) {
      let command = tera
        .render_str(command, context)
        .map_err(|error| variable_error(key, format_tera_error(&error), secret))?;
      if secret {
        debug!("Processing secret shell-backed variable '{}'", key);
      } else {
        debug!("Processing shell-backed variable '{}': '{}'", key, command);
      }
      return shell
        .execute(&command)
        .map(Value::String)
        .map_err(|error| variable_error(key, format_tera_error(&error), secret));
    }

    let val = match value {
      Value::String(value) => value.trim().to_owned(),
      value => value.to_string().trim().to_owned(),
    };

    if !self.is_template(&val) {
      return Ok(value.clone());
    }

    if secret {
      debug!("Processing secret template variable '{}'", key);
    } else {
      debug!("Processing template variable '{}' with value: '{}'", key, val);
    }
    let res = tera
      .render_str(&val, context)
      .map_err(|error| variable_error(key, format_tera_error(&error), secret))?;
    let res = res.trim_matches('"').to_owned(); // remove extra quotes in value

    let val = match serde_json::from_str(&res) {
      Ok(val) => val,
      Err(_) => Value::String(res),
    };

    Ok(val)
  }

  fn is_template(&self, value: &str) -> bool {
    TEMPLATE_REGEX.is_match(value)
  }

  pub fn to_hashmap(&self) -> HashMap<String, Value> {
    self
      .values
      .iter()
      .map(|(key, value)| (key.clone(), value.clone()))
      .collect()
  }

  /// Returns the hierarchy as one map, with child values overriding parent values.
  pub(crate) fn to_merged_hashmap(&self) -> HashMap<String, Value> {
    let mut result = HashMap::new();

    for layer in self.collect_context_chain() {
      result.extend(layer.values);
    }

    result
  }

  /// Names of values that plugins must redact from their diagnostic logs.
  pub(crate) fn secret_names(&self) -> Vec<String> {
    self.secrets.iter().cloned().collect()
  }
}

fn shell_command(value: &Value) -> Option<&str> {
  let object = value.as_object()?;
  (object.len() == 1).then(|| object.get("sh")?.as_str()).flatten()
}

fn values_as_environment(values: &IndexMap<String, Value>) -> HashMap<String, String> {
  // Only scalar values have an unambiguous representation in a process environment.
  values
    .iter()
    .filter_map(|(key, value)| match value {
      Value::String(value) => Some((key.clone(), value.clone())),
      Value::Bool(_) | Value::Number(_) => Some((key.clone(), value.to_string())),
      _ => None,
    })
    .collect()
}

/// Converts arbitrary runtime mappings without a JSON text serialization round-trip.
fn serialized_values<T: Serialize + ?Sized>(value: &T) -> IndexMap<String, Value> {
  match serde_json::to_value(value) {
    Ok(Value::Object(values)) => values.into_iter().collect(),
    _ => IndexMap::new(),
  }
}

fn variable_error(key: &str, message: String, secret: bool) -> ExecutorError {
  if secret {
    // Parser or shell errors may embed the literal value or producer command in their source chain.
    ExecutorError::VariableExpandError(key.to_owned(), "failed to expand secret variable".to_owned())
  } else {
    ExecutorError::VariableExpandError(key.to_owned(), message)
  }
}

/// Collects resolved values whose names are marked secret in the current flattened scope.
fn secret_values(values: &IndexMap<String, Value>, secrets: &HashSet<String>) -> Vec<String> {
  let mut redactions = Vec::new();
  for value in secrets.iter().filter_map(|key| values.get(key)) {
    collect_value_redactions(value, &mut redactions);
  }
  redactions
}

impl Display for Vars {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    writeln!(f, "[")?;
    for (i, (key, value)) in self.iter().enumerate() {
      if i > 0 {
        writeln!(f, ",")?;
      }
      if self.secrets.contains(&key) {
        write!(f, "  \"{}\": \"*****\"", key)?;
      } else {
        write!(f, "  \"{}\": {}", key, value)?;
      }
    }
    writeln!(f, "\n]")
  }
}

// A derived implementation would recursively expose secret values from this scope and its parents.
impl Debug for Vars {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    let values: IndexMap<_, _> = self
      .values
      .iter()
      .map(|(key, value)| {
        let value = if self.secrets.contains(key) {
          Value::String("*****".to_owned())
        } else {
          value.clone()
        };
        (key.clone(), value)
      })
      .collect();

    f.debug_struct("Vars")
      .field("values", &values)
      .field("parent", &self.parent)
      .field("dir", &self.dir)
      .field("expanded", &self.expanded)
      .finish()
  }
}

impl From<Context> for Vars {
  fn from(context: Context) -> Self {
    Self {
      values: context
        .into_json()
        .as_object()
        .map(|values| values.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
        .unwrap_or_default(),
      // Tera Context contains values only and cannot carry secret metadata.
      secrets: HashSet::new(),
      parent: None,
      dir: None,
      expanded: false,
    }
  }
}

impl From<Vars> for Context {
  fn from(vars: Vars) -> Self {
    Context::from_serialize(vars.values).unwrap_or_default()
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use super::*;
  use serde_json::json;
  use tempfile::TempDir;

  #[test]
  fn test_new_vars() {
    let vars = Vars::new();
    assert!(vars.parent.is_none());
    assert!(!vars.expanded);
    assert!(vars.values.is_empty());
  }

  #[test]
  fn test_with_parent() {
    let parent = Vars::new();
    let vars = Vars::with_parent(parent);
    assert!(vars.parent.is_some());
    assert!(!vars.expanded);
  }

  #[test]
  fn test_with_value() {
    let value = json!({
      "key": "value",
      "number": 42
    });
    let vars = Vars::with_value(&value);

    assert_eq!(vars.get("key").unwrap(), &Value::String("value".to_string()));
    assert_eq!(vars.get("number").unwrap(), &Value::Number(42.into()));
  }

  #[test]
  fn test_with_value_and_parent() {
    let parent = Vars::with_value(json!({"parent_key": "parent_value"}));
    let vars = Vars::with_value_and_parent(json!({"child_key": "child_value"}), parent);

    assert!(vars.parent.is_some());
    assert_eq!(
      vars.get("child_key").unwrap(),
      &Value::String("child_value".to_string())
    );
  }

  #[test]
  fn test_insert() {
    let mut vars = Vars::new();
    vars.insert("key", &"value");
    assert_eq!(vars.get("key").unwrap(), &Value::String("value".to_string()));
  }

  #[test]
  fn test_extend() {
    let mut vars = Vars::new();
    let mut context = Context::new();
    context.insert("key", &"value");
    vars.extend(context);

    assert_eq!(vars.get("key").unwrap(), &Value::String("value".to_string()));
  }

  #[test]
  fn test_extend_with() {
    let mut vars = Vars::new();
    let value = json!({
      "key": "value"
    });
    vars.extend_with(&value);

    assert_eq!(vars.get("key").unwrap(), &Value::String("value".to_string()));
  }

  #[test]
  fn test_vars_iterator() {
    let vars = Vars::with_value(json!({
      "key1": "value1",
      "key2": "value2"
    }));

    let items: Vec<_> = vars.iter().collect();
    assert_eq!(items.len(), 2);
    assert!(items
      .iter()
      .any(|(k, v)| k == "key1" && v == &Value::String("value1".to_string())));
    assert!(items
      .iter()
      .any(|(k, v)| k == "key2" && v == &Value::String("value2".to_string())));
  }

  #[test]
  fn test_display() {
    let vars = Vars::with_value(json!({
      "key": "value",
      "number": 42
    }));

    let display = format!("{}", vars);

    assert!(display.contains("\"key\": \"value\""));
    assert!(display.contains("\"number\": 42"));
  }

  #[test]
  fn test_is_template() {
    let vars = Vars::new();
    assert!(vars.is_template("\"{{ template }}\""));
    assert!(!vars.is_template("normal string"));
    assert!(vars.is_template("{{incomplete}}"));
  }

  #[test]
  fn test_context_conversion() {
    let mut context = Context::new();
    context.insert("key", &"value");

    // Test From<Context> for Vars
    let vars: Vars = context.clone().into();
    assert_eq!(vars.get("key").unwrap(), &Value::String("value".to_string()));

    // Test From<Vars> for Context
    let context_back: Context = vars.into();
    assert_eq!(context_back.get("key").unwrap(), &Value::String("value".to_string()));
  }

  #[tokio::test]
  async fn test_expand_simple() {
    let mut vars = Vars::with_value(json!({
      "name": "{{ 'John' }}",
    }));

    vars.expand(true).await.unwrap();
    assert_eq!(vars.get("name").unwrap(), &Value::String("John".to_string()));
  }

  #[tokio::test]
  async fn test_expand_with_parent() {
    let parent = Vars::with_value(json!({
      "first": "John"
    }));
    let mut vars = Vars::with_value_and_parent(
      json!({
        "full": "{{ first }} Doe"
      }),
      parent,
    );

    vars.expand(true).await.unwrap();
    assert_eq!(vars.get("full").unwrap(), &Value::String("John Doe".to_string()));
  }

  #[tokio::test]
  async fn expands_variables_in_declaration_order() {
    let values: octa_octafile::Vars = serde_yml::from_str(
      r#"
      GREETING: Hello
      TARGET: World
      MESSAGE: "{{ GREETING }} {{ TARGET }}"
      COMPLETE: "{{ MESSAGE }}!"
      "#,
    )
    .unwrap();
    let mut vars = Vars::with_variables(values);

    vars.expand(true).await.unwrap();

    assert_eq!(vars.get("MESSAGE"), Some(&Value::String("Hello World".to_owned())));
    assert_eq!(vars.get("COMPLETE"), Some(&Value::String("Hello World!".to_owned())));
  }

  #[tokio::test]
  async fn rejects_forward_references_in_the_same_layer() {
    let values: octa_octafile::Vars = serde_yml::from_str(
      r#"
      MESSAGE: "{{ TARGET }}"
      TARGET: World
      "#,
    )
    .unwrap();
    let mut vars = Vars::with_variables(values);

    assert!(vars.expand(true).await.is_err());
  }

  #[tokio::test]
  async fn expands_and_redacts_secret_variables() {
    let values: octa_octafile::Vars = serde_yml::from_str(
      r#"
      PREFIX: token
      API_KEY:
        value: "{{ PREFIX }}-123"
        secret: true
      "#,
    )
    .unwrap();
    let mut vars = Vars::with_variables(values);

    assert!(!format!("{vars:?}").contains("token-123"));

    vars.expand(true).await.unwrap();

    assert_eq!(vars.get("API_KEY"), Some(&Value::String("token-123".to_owned())));
    assert_eq!(vars.secret_names(), vec!["API_KEY"]);
    assert!(!format!("{vars}").contains("token-123"));
    assert!(!format!("{vars:?}").contains("token-123"));
    assert!(format!("{vars}").contains("*****"));
  }

  #[tokio::test]
  async fn expands_secret_shell_variables() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("token.txt"), "shell-secret").unwrap();
    #[cfg(windows)]
    let command = "type token.txt";
    #[cfg(not(windows))]
    let command = "cat token.txt";
    let values: octa_octafile::Vars = serde_yml::from_str(&format!(
      "TOKEN:\n  sh: {command}\n  secret: true\nDISPLAY: '{{{{ TOKEN }}}}'\n"
    ))
    .unwrap();
    let mut vars = Vars::with_variables(values);
    vars.set_dir(temp_dir.path());

    vars.expand(false).await.unwrap();

    assert_eq!(vars.get("TOKEN"), Some(&Value::String("shell-secret".to_owned())));
    assert_eq!(vars.get("DISPLAY"), Some(&Value::String("shell-secret".to_owned())));
    assert_eq!(vars.secret_names(), vec!["TOKEN"]);
  }

  #[tokio::test]
  async fn non_secret_override_removes_secret_metadata() {
    let parent_values: octa_octafile::Vars = serde_yml::from_str("TOKEN:\n  value: hidden\n  secret: true\n").unwrap();
    let child_values: octa_octafile::Vars = serde_yml::from_str("TOKEN: visible\n").unwrap();
    let parent = Vars::with_variables(parent_values);
    let mut vars = Vars::with_variables_and_parent(child_values, parent);

    vars.expand(true).await.unwrap();

    assert_eq!(vars.get("TOKEN"), Some(&Value::String("visible".to_owned())));
    assert!(vars.secret_names().is_empty());
  }

  #[tokio::test]
  async fn secret_shell_errors_do_not_expose_commands_or_stderr() {
    #[cfg(windows)]
    let command = "echo literal-secret 1>&2 && exit /b 1";
    #[cfg(not(windows))]
    let command = "echo literal-secret >&2; exit 1";
    let values: octa_octafile::Vars =
      serde_yml::from_str(&format!("TOKEN:\n  sh: '{command}'\n  secret: true\n")).unwrap();
    let mut vars = Vars::with_variables(values);

    let error = vars.expand(false).await.unwrap_err().to_string();

    assert!(error.contains("TOKEN"));
    assert!(!error.contains("literal-secret"));
  }

  #[tokio::test]
  async fn shell_errors_redact_inherited_secret_values() {
    #[cfg(windows)]
    let command = "echo {{ TOKEN }} 1>&2 && exit /b 1";
    #[cfg(not(windows))]
    let command = "echo {{ TOKEN }} >&2; exit 1";
    let values: octa_octafile::Vars = serde_yml::from_str(&format!(
      "TOKEN:\n  value: literal-secret\n  secret: true\nCHECK:\n  sh: '{command}'\n"
    ))
    .unwrap();
    let mut vars = Vars::with_variables(values);

    let error = vars.expand(false).await.unwrap_err().to_string();

    assert!(error.contains("CHECK"));
    assert!(!error.contains("literal-secret"));
  }

  #[tokio::test]
  async fn expands_structured_shell_values_and_shell_filters() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("value.txt"), "dynamic").unwrap();
    #[cfg(windows)]
    let command = "type value.txt";
    #[cfg(not(windows))]
    let command = "cat value.txt";
    let mut vars = Vars::with_value(json!({
      "STRUCTURED": { "sh": command },
      "FILTERED": format!("prefix-{{{{ \"{command}\" | shell }}}}"),
    }));
    vars.set_dir(temp_dir.path());

    vars.expand(false).await.unwrap();

    assert_eq!(vars.get("STRUCTURED"), Some(&Value::String("dynamic".to_owned())));
    assert_eq!(vars.get("FILTERED"), Some(&Value::String("prefix-dynamic".to_owned())));
  }
}
