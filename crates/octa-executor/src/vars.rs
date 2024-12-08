use std::{
  collections::HashMap,
  fmt::{Display, Formatter},
  sync::Arc,
};

use lazy_static::lazy_static;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tera::{Context, Tera, Value};
use tokio::sync::RwLock;
use tracing::debug;

use crate::{
  error::{ExecutorError, ExecutorResult},
  function::ExecuteShell,
};

lazy_static! {
  static ref TEMPLATE_ENGINE: RwLock<Tera> = {
    let mut tera = Tera::default();
    tera.register_function("shell", ExecuteShell);

    RwLock::new(tera)
  };
}

#[derive(Clone, Debug)]
pub struct Vars {
  context: Context,
  parent: Option<Arc<Vars>>,
  interpolated: bool,
}

impl Vars {
  pub fn new() -> Self {
    Vars {
      context: Context::default(),
      parent: None,
      interpolated: false,
    }
  }

  pub fn with_parent(parent: Vars) -> Self {
    Self {
      context: Context::default(),
      parent: Some(Arc::new(parent)),
      interpolated: false,
    }
  }

  pub fn with_value<T: Serialize>(value: T) -> Self {
    let mut vars = Self::new();
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
    self.interpolated = false;
  }

  pub fn set_parent(&mut self, parent: Option<Vars>) {
    self.parent = parent.map(Arc::new);
    self.interpolated = false;
  }

  pub fn insert<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) {
    self.context.insert(key, value);
    self.interpolated = false;
  }

  pub fn get(&self, key: &str) -> Option<&Value> {
    self.context.get(key)
  }

  pub fn extend(&mut self, source: Context) {
    self.context.extend(source);
    self.interpolated = false;
  }

  pub fn extend_with<T: Serialize>(&mut self, value: &T) {
    if let Ok(context) = Context::from_serialize(value) {
      self.extend(context.into());
    }
  }

  pub fn iter(&self) -> VarsIter {
    let map = self.to_hashmap();
    VarsIter::new(map)
  }

  pub async fn interpolate(&mut self) -> ExecutorResult<()> {
    if self.interpolated {
      return Ok(());
    }

    let contexts = self.collect_context_chain();
    let processed_context = self.process_context_chain(contexts).await?;
    self.context = processed_context;
    self.interpolated = true;

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

  async fn process_context_chain(&self, contexts: Vec<Context>) -> ExecutorResult<Context> {
    let mut accumulated = Context::new();

    for context in contexts {
      let processed = self.process_single_context(context, &accumulated).await?;
      accumulated.extend(processed);
    }

    Ok(accumulated)
  }

  async fn process_single_context(&self, context: Context, parent: &Context) -> ExecutorResult<Context> {
    let mut processed = Context::new();
    let vars = Vars {
      context,
      parent: None,
      interpolated: false,
    };

    for (key, value) in vars.iter() {
      let processed_value = self.process_template_value(&key, &value, parent).await?;
      processed.insert(&key, &processed_value);
    }

    Ok(processed)
  }

  async fn process_template_value(&self, key: &str, value: &Value, context: &Context) -> ExecutorResult<String> {
    let val = value.to_string().trim().to_owned();

    if !self.is_template(&val) {
      return Ok(val);
    }

    debug!("Processing template variable '{}' with value: '{}'", key, val);
    let mut template = TEMPLATE_ENGINE.write().await;
    template
      .render_str(&val, context)
      .map_err(|e| ExecutorError::VariableInterpolateError(val, e.to_string()))
  }

  fn is_template(&self, value: &str) -> bool {
    value.starts_with("\"{{") && value.ends_with("}}\"")
  }

  fn to_hashmap(&self) -> HashMap<String, Value> {
    self
      .context
      .clone()
      .into_json()
      .as_object()
      .map(|map| map.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect())
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
      write!(f, "  \"{}\": \"{}\"", key, value)?;
    }
    writeln!(f, "\n]")
  }
}

impl From<Context> for Vars {
  fn from(context: Context) -> Self {
    Self {
      context,
      parent: None,
      interpolated: false,
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
