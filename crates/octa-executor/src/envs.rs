use std::{
  borrow::Cow,
  collections::HashMap,
  env,
  fmt::{Display, Formatter},
  sync::Arc,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::debug;

use crate::error::ExecutorResult;

type EnvContext = HashMap<String, String>;

#[derive(Clone, Debug, Default)]
pub struct Envs {
  context: EnvContext,       // Current environments
  parent: Option<Arc<Envs>>, // Link to parent environments
  expanded: bool,            // Inindicator that the values have been expanded
}

impl PartialEq for Envs {
  fn eq(&self, other: &Self) -> bool {
    self.context == other.context
  }
}

impl Eq for Envs {}

impl Envs {
  pub fn new() -> Self {
    Self {
      context: HashMap::default(),
      parent: None,
      expanded: false,
    }
  }

  pub fn with_parent(parent: Envs) -> Self {
    Self {
      context: HashMap::default(),
      parent: Some(Arc::new(parent)),
      expanded: false,
    }
  }

  pub fn with_value(value: EnvContext) -> Self {
    let mut envs = Self::default();
    envs.set_value(value);
    envs
  }

  pub fn with_value_and_parent(value: EnvContext, parent: Envs) -> Self {
    let mut envs = Self::with_parent(parent);
    envs.set_value(value);
    envs
  }

  pub fn set_parent(&mut self, parent: Option<Envs>) {
    self.parent = parent.map(Arc::new);
    self.expanded = false;
  }

  pub fn set_value(&mut self, value: EnvContext) {
    self.context = value;
    self.expanded = false;
  }

  pub fn get(&self, key: &str) -> Option<&String> {
    self.context.get(key)
  }

  pub fn insert<T: AsRef<str>>(&mut self, key: &T, value: &T) {
    self
      .context
      .insert(key.as_ref().to_string(), value.as_ref().to_string());
    self.expanded = false;
  }

  pub fn extend(&mut self, source: EnvContext) {
    self.context.extend(source);
    self.expanded = false;
  }

  pub fn iter(&self) -> EnvsIter {
    EnvsIter::new(self.context.clone())
  }

  pub async fn expand(&mut self) -> ExecutorResult<()> {
    if self.expanded {
      return Ok(());
    }

    let contexts = self.collect_context_chain();
    let processed_context = self.process_context_chain(contexts).await?;
    self.context = processed_context;
    self.expanded = true;

    Ok(())
  }

  fn collect_context_chain(&self) -> Vec<EnvContext> {
    let mut contexts = Vec::new();
    let mut current = Some(self);

    while let Some(envs) = current {
      contexts.push(envs.context.clone());
      current = envs.parent.as_ref().map(|p| p.as_ref());
    }

    contexts.into_iter().rev().collect()
  }

  async fn process_context_chain(&self, contexts: Vec<EnvContext>) -> ExecutorResult<EnvContext> {
    let mut accumulated = EnvContext::new();

    for context in contexts {
      let processed = self.process_single_context(context, &accumulated).await?;
      accumulated.extend(processed);
    }

    Ok(accumulated)
  }

  async fn process_single_context(&self, context: EnvContext, parent: &EnvContext) -> ExecutorResult<EnvContext> {
    let mut processed = EnvContext::new();
    let envs = Envs {
      context,
      parent: None,
      expanded: false,
    };

    for (key, value) in envs.iter() {
      let processed_value = self.process_template_value(&key, &value, parent).await?;
      processed.insert(key, processed_value);
    }

    Ok(processed)
  }

  async fn process_template_value(&self, key: &str, value: &str, context: &EnvContext) -> ExecutorResult<String> {
    let val = value.trim().to_owned();

    let get_env = |name: &str| match context.get(name) {
      Some(val) => Some(Cow::Borrowed(val.as_str())),
      None => match env::var(name) {
        Ok(val) => Some(Cow::Owned(val)),
        Err(_) => None,
      },
    };

    debug!("Processing environment '{}' with value: '{}'", key, val);
    let val = shellexpand::env_with_context_no_errors(&val, get_env);

    Ok(val.to_string())
  }
}

impl Display for Envs {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    writeln!(f, "[")?;
    for (i, (key, value)) in self.iter().enumerate() {
      if i > 0 {
        writeln!(f, ",")?;
      }
      write!(f, "  \"{}\": \"{}\"", key, value)?;
    }
    writeln!(f, "\n]")
  }
}

impl From<EnvContext> for Envs {
  fn from(context: EnvContext) -> Self {
    Self {
      context,
      parent: None,
      expanded: false,
    }
  }
}

impl From<Envs> for EnvContext {
  fn from(envs: Envs) -> Self {
    envs.context
  }
}

impl Serialize for Envs {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    self.context.clone().serialize(serializer)
  }
}

impl<'de> Deserialize<'de> for Envs {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let mut envs = Envs::new();
    envs.context = EnvContext::deserialize(deserializer)?;
    Ok(envs)
  }
}

pub struct EnvsIter {
  map: EnvContext,
  keys: Vec<String>,
  position: usize,
}

impl EnvsIter {
  fn new(map: EnvContext) -> Self {
    let keys: Vec<String> = map.keys().cloned().collect();
    Self { map, keys, position: 0 }
  }
}

impl Iterator for EnvsIter {
  type Item = (String, String);

  fn next(&mut self) -> Option<Self::Item> {
    self.keys.get(self.position).map(|key| {
      self.position += 1;
      (key.clone(), self.map.get(key).unwrap().clone())
    })
  }
}

impl IntoIterator for Envs {
  type Item = (String, String);
  type IntoIter = EnvsIter;

  fn into_iter(self) -> Self::IntoIter {
    EnvsIter::new(self.context)
  }
}

impl IntoIterator for &Envs {
  type Item = (String, String);
  type IntoIter = EnvsIter;

  fn into_iter(self) -> Self::IntoIter {
    EnvsIter::new(self.context.clone())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_new_envs() {
    let envs = Envs::new();
    assert!(envs.parent.is_none());
    assert!(!envs.expanded);
    assert_eq!(envs.context, EnvContext::new());
  }

  #[test]
  fn test_with_parent() {
    let parent = Envs::new();
    let envs = Envs::with_parent(parent);
    assert!(envs.parent.is_some());
    assert!(!envs.expanded);
  }

  #[test]
  fn test_with_value() {
    let mut map = EnvContext::new();
    map.insert("key".to_owned(), "value".to_owned());

    let envs = Envs::with_value(map);

    assert_eq!(envs.get("key").unwrap(), &"value".to_string());
  }

  #[test]
  fn test_with_value_and_parent() {
    let mut parent_map = EnvContext::new();
    parent_map.insert("parent_key".to_owned(), "parent_value".to_owned());

    let mut map = EnvContext::new();
    map.insert("child_key".to_owned(), "child_value".to_owned());

    let parent = Envs::with_value(parent_map);
    let envs = Envs::with_value_and_parent(map, parent);

    assert!(envs.parent.is_some());
    assert_eq!(envs.get("child_key").unwrap(), &"child_value".to_string());
  }

  #[test]
  fn test_insert() {
    let mut envs = Envs::new();
    envs.insert(&"key".to_string(), &"value".to_string());
    assert_eq!(envs.get("key").unwrap(), &"value".to_string());
  }

  #[test]
  fn test_extend() {
    let mut envs = Envs::new();
    let mut context = EnvContext::new();
    context.insert("key".to_string(), "value".to_string());
    envs.extend(context);

    assert_eq!(envs.get("key").unwrap(), &"value".to_string());
  }

  #[test]
  fn test_envs_iterator() {
    let mut map = EnvContext::new();
    map.insert("key1".to_owned(), "value1".to_owned());
    map.insert("key2".to_owned(), "value2".to_owned());

    let envs = Envs::with_value(map);

    let items: Vec<_> = envs.iter().collect();
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|(k, v)| k == "key1" && v == &"value1".to_string()));
    assert!(items.iter().any(|(k, v)| k == "key2" && v == &"value2".to_string()));
  }

  #[test]
  fn test_serialize_deserialize() {
    let mut map = EnvContext::new();
    map.insert("key".to_owned(), "value".to_owned());

    let original = Envs::with_value(map);

    let serialized = serde_json::to_string(&original).unwrap();
    let deserialized: Envs = serde_json::from_str(&serialized).unwrap();

    assert_eq!(original, deserialized);
  }

  #[test]
  fn test_display() {
    let mut map = EnvContext::new();
    map.insert("key".to_owned(), "value".to_owned());

    let envs = Envs::with_value(map);

    let display = format!("{}", envs);

    assert!(display.contains("\"key\": \"value\""));
  }

  #[test]
  fn test_context_conversion() {
    let mut context = EnvContext::new();
    context.insert("key".to_owned(), "value".to_owned());

    // Test From<Context> for Envs
    let envs: Envs = context.clone().into();
    assert_eq!(envs.get("key").unwrap(), &"value".to_string());

    // Test From<Envs> for Context
    let context_back: EnvContext = envs.into();
    assert_eq!(context_back.get("key").unwrap(), &"value".to_string());
  }

  #[tokio::test]
  async fn test_expand_simple() {
    let mut context = EnvContext::new();
    context.insert("name".to_owned(), "John".to_owned());
    context.insert("key".to_owned(), "$name".to_owned());

    let mut envs = Envs::with_value(context);

    envs.expand().await.unwrap();
    assert_eq!(envs.get("name").unwrap(), &"John".to_string());
  }

  #[tokio::test]
  async fn test_expand_with_parent() {
    let mut parent_context = EnvContext::new();
    parent_context.insert("first".to_owned(), "John".to_owned());
    let mut context = EnvContext::new();
    context.insert("full".to_owned(), "$first Doe".to_owned());

    let parent = Envs::with_value(parent_context);

    let mut envs = Envs::with_value_and_parent(context, parent);

    envs.expand().await.unwrap();
    assert_eq!(envs.get("full").unwrap(), &"John Doe".to_string());
  }
}
