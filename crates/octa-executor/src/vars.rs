use std::{
  collections::HashMap,
  fmt::{Display, Formatter},
  sync::Arc,
};

use lazy_static::lazy_static;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tera::{Context, Tera, Value};
use tracing::debug;

use crate::{
  error::{ExecutorError, ExecutorResult},
  function::{ExecuteShell, ExecuteShellDry},
};

lazy_static! {
  static ref TEMPLATE_REGEX: Regex = Regex::new(r"\{\{\s*[^{}]+\s*\}\}").unwrap();
}

#[derive(Clone, Debug, Default)]
pub struct Vars {
  context: Context,          // Tera context for current variables
  parent: Option<Arc<Vars>>, // Link to parent variables
  expanded: bool,            // Inindicator that the values have been expanded
}

impl PartialEq for Vars {
  fn eq(&self, other: &Self) -> bool {
    self.context == other.context
  }
}

impl Eq for Vars {}

impl Vars {
  pub fn new() -> Self {
    Self {
      context: Context::default(),
      parent: None,
      expanded: false,
    }
  }

  pub fn with_parent(parent: Vars) -> Self {
    Self {
      context: Context::default(),
      parent: Some(Arc::new(parent)),
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

  pub fn set_value<T: Serialize>(&mut self, value: T) {
    self.context = Context::from_serialize(value).unwrap_or_default();
    self.expanded = false;
  }

  pub fn set_parent(&mut self, parent: Option<Vars>) {
    self.parent = parent.map(Arc::new);
    self.expanded = false;
  }

  pub fn insert<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) {
    self.context.insert(key, value);
    self.expanded = false;
  }

  pub fn get(&self, key: &str) -> Option<&Value> {
    self.context.get(key)
  }

  pub fn extend(&mut self, source: Context) {
    self.context.extend(source);
    self.expanded = false;
  }

  pub fn extend_with<T: Serialize>(&mut self, value: &T) {
    if let Ok(context) = Context::from_serialize(value) {
      self.extend(context);
      self.expanded = false;
    }
  }

  pub fn iter(&self) -> VarsIter {
    let map = self.to_hashmap();
    VarsIter::new(map)
  }

  pub async fn expand(&mut self, dry: bool) -> ExecutorResult<()> {
    let mut tera = Tera::default();
    if dry {
      tera.register_function("shell", ExecuteShellDry);
    } else {
      tera.register_function("shell", ExecuteShell);
    }

    if self.expanded {
      return Ok(());
    }

    let contexts = self.collect_context_chain();
    let processed_context = self.process_context_chain(contexts, &mut tera).await?;
    self.context = processed_context;
    self.expanded = true;

    Ok(())
  }

  fn collect_context_chain(&self) -> Vec<Context> {
    let mut contexts = Vec::new();
    let mut current = Some(self);

    while let Some(vars) = current {
      contexts.push(vars.context.clone());
      current = vars.parent.as_ref().map(|p| p.as_ref());
    }

    contexts.into_iter().rev().collect()
  }

  async fn process_context_chain(&self, contexts: Vec<Context>, tera: &mut Tera) -> ExecutorResult<Context> {
    let mut accumulated = Context::new();

    for context in contexts {
      let processed = self.process_single_context(context, &accumulated, tera).await?;
      accumulated.extend(processed);
    }

    Ok(accumulated)
  }

  async fn process_single_context(
    &self,
    context: Context,
    parent: &Context,
    tera: &mut Tera,
  ) -> ExecutorResult<Context> {
    let mut processed = Context::new();
    let vars = Vars {
      context,
      parent: None,
      expanded: false,
    };

    for (key, value) in vars.iter() {
      let processed_value = self.process_template_value(&key, &value, parent, tera).await?;
      processed.insert(&key, &processed_value);
    }

    Ok(processed)
  }

  async fn process_template_value(
    &self,
    key: &str,
    value: &Value,
    context: &Context,
    tera: &mut Tera,
  ) -> ExecutorResult<Value> {
    let val = value.to_string().trim().to_owned();

    if !self.is_template(&val) {
      return Ok(value.clone());
    }

    debug!("Processing template variable '{}' with value: '{}'", key, val);
    let res = tera
      .render_str(&val, context)
      .map_err(|e| ExecutorError::VariableExpandError(val, e.to_string()))?;
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

  fn to_hashmap(&self) -> HashMap<String, Value> {
    self
      .context
      .clone()
      .into_json()
      .as_object()
      .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
      .unwrap_or_default()
  }
}

impl Display for Vars {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    writeln!(f, "[")?;
    for (i, (key, value)) in self.iter().enumerate() {
      if i > 0 {
        writeln!(f, ",")?;
      }
      write!(f, "  \"{}\": {}", key, value)?;
    }
    writeln!(f, "\n]")
  }
}

impl From<Context> for Vars {
  fn from(context: Context) -> Self {
    Self {
      context,
      parent: None,
      expanded: false,
    }
  }
}

impl From<Vars> for Context {
  fn from(vars: Vars) -> Self {
    vars.context
  }
}

impl Serialize for Vars {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    self.context.clone().into_json().serialize(serializer)
  }
}

impl<'de> Deserialize<'de> for Vars {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let map = HashMap::<String, Value>::deserialize(deserializer)?;
    let mut vars = Vars::new();
    vars.context = Context::from_serialize(map).unwrap_or_default();
    Ok(vars)
  }
}

pub struct VarsIter {
  map: HashMap<String, Value>,
  keys: Vec<String>,
  position: usize,
}

impl VarsIter {
  fn new(map: HashMap<String, Value>) -> Self {
    let keys: Vec<String> = map.keys().cloned().collect();
    Self { map, keys, position: 0 }
  }
}

impl Iterator for VarsIter {
  type Item = (String, Value);

  fn next(&mut self) -> Option<Self::Item> {
    self.keys.get(self.position).map(|key| {
      self.position += 1;
      (key.clone(), self.map.get(key).unwrap().clone())
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn test_new_vars() {
    let vars = Vars::new();
    assert!(vars.parent.is_none());
    assert!(!vars.expanded);
    assert_eq!(vars.context, Context::new());
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
    let parent = Vars::with_value(&json!({"parent_key": "parent_value"}));
    let vars = Vars::with_value_and_parent(&json!({"child_key": "child_value"}), parent);

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
    let vars = Vars::with_value(&json!({
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
  fn test_serialize_deserialize() {
    let original = Vars::with_value(&json!({
      "key": "value",
      "number": 42
    }));

    let serialized = serde_json::to_string(&original).unwrap();
    let deserialized: Vars = serde_json::from_str(&serialized).unwrap();

    assert_eq!(original, deserialized);
  }

  #[test]
  fn test_display() {
    let vars = Vars::with_value(&json!({
      "key": "value",
      "number": 42
    }));

    let display = format!("{}", vars);
    println!("{}", display);

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
    let mut vars = Vars::with_value(&json!({
      "name": "{{ 'John' }}",
    }));

    vars.expand(true).await.unwrap();
    assert_eq!(vars.get("name").unwrap(), &Value::String("John".to_string()));
  }

  #[tokio::test]
  async fn test_expand_with_parent() {
    let parent = Vars::with_value(&json!({
      "first": "John"
    }));
    let mut vars = Vars::with_value_and_parent(
      &json!({
        "full": "{{ first }} Doe"
      }),
      parent,
    );

    vars.expand(true).await.unwrap();
    assert_eq!(vars.get("full").unwrap(), &Value::String("John Doe".to_string()));
  }
}
