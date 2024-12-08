use std::{
  collections::HashMap,
  fmt::{Display, Formatter, Result as FmtResult},
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
  static ref VARS_TEMPLATES: RwLock<Tera> = {
    let mut tera = Tera::default();
    tera.register_function("shell", ExecuteShell);

    RwLock::new(tera)
  };
}

#[derive(Clone, Debug)]
pub struct Vars {
  context: Context,
  _parent: Option<Arc<Vars>>,
  _child: Option<Arc<Vars>>,
  _fullfiled: bool,
}

impl Vars {
  pub fn new() -> Self {
    Vars {
      context: Context::default(),
      _parent: None,
      _child: None,
      _fullfiled: false,
    }
  }

  pub fn set_value<T: Serialize>(&mut self, value: T) {
    self.context = Context::from_serialize(value).unwrap_or_default();
  }

  pub fn set_parent(&mut self, parent: Option<Vars>) {
    self._parent = match parent {
      Some(parent) => Some(Arc::new(parent)),
      None => None,
    };
  }

  pub fn insert<T: Serialize + ?Sized>(&mut self, key: &str, value: &T) {
    self.context.insert(key, value);
  }

  pub fn get(&self, key: &str) -> Option<&Value> {
    self.context.get(key)
  }

  pub fn extend(&mut self, source: Vars) {
    self.context.extend(source.context);
  }

  pub fn parent(&self) -> Option<&Arc<Vars>> {
    self._parent.as_ref()
  }

  pub fn root(&self) -> &Vars {
    let mut current = self;
    while let Some(parent) = current.parent() {
      current = parent;
    }
    current
  }

  pub fn extend_with<T: Serialize>(&mut self, value: &T) {
    if let Ok(context) = Context::from_serialize(value) {
      self.extend(context.into());
    }
  }

  pub fn iter(self) -> VarsIter {
    let map = self.to_hashmap();
    let keys: Vec<String> = map.keys().cloned().collect();

    VarsIter { map, keys, current: 0 }
  }

  pub async fn interpolate(&mut self) -> ExecutorResult<()> {
    // Get the chain of contexts from root to current
    let mut contexts = Vec::new();
    contexts.push(self.context.clone());

    // Collect contexts from current to root
    let mut current = self._parent.as_ref();
    while let Some(parent) = current {
      contexts.push(parent.context.clone());
      current = parent._parent.as_ref();
    }

    // Reverse to process from root to current
    contexts.reverse();

    // Process each context level with accumulated context
    let mut accumulated_context = Context::new();
    for context in contexts {
      // Create temporary Vars for processing
      let mut current_vars = Vars {
        context,
        _parent: None,
        _child: None,
        _fullfiled: false,
      };

      // Process current level with accumulated context
      current_vars.process_value(&accumulated_context).await?;

      // Add processed variables to accumulated context
      accumulated_context.extend(current_vars.context);
    }

    // Update self with final processed values
    self.context = accumulated_context;

    Ok(())
  }

  async fn process_value(&mut self, parent_context: &Context) -> ExecutorResult<()> {
    let mut updates = HashMap::new();

    for (key, value) in self.clone().iter() {
      let val = value.to_string().trim().to_owned();
      debug!("Processing variable '{}' with value: '{}'", key, val);

      if val.starts_with("\"{{") && val.ends_with("}}\"") {
        debug!("Detected template in variable '{}'", key);

        let mut template = VARS_TEMPLATES.write().await;
        let res = template
          .render_str(&val, parent_context)
          .map_err(|e| ExecutorError::VariableInterpolateError(val, e.to_string()))?;

        updates.insert(key, res);
      }
    }

    // Apply all updates at once
    for (key, value) in updates {
      self.context.insert(&key, &value);
    }

    Ok(())
  }

  fn to_hashmap(&self) -> HashMap<String, Value> {
    let json = self.context.clone().into_json();
    match json.as_object() {
      Some(map) => map.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect(),
      None => HashMap::new(),
    }
  }
}

impl Display for Vars {
  fn fmt(&self, f: &mut Formatter) -> FmtResult {
    write!(f, "[\n")?;

    let mut first = true; // Flag to handle commas between key-value pairs
    for (key, value) in self.to_hashmap() {
      if !first {
        write!(f, ", ")?;
      }

      write!(f, "\"{}\": \"{}\"\n", key, value)?;
      first = false;
    }

    write!(f, "\n]")
  }
}

impl From<Context> for Vars {
  fn from(context: Context) -> Self {
    let json = context.clone().into_json();
    let mut vars = Vars::new();

    let map: HashMap<String, Value> = match json.as_object() {
      Some(map) => map.iter().map(|(k, v)| (k.clone(), Value::from(v.clone()))).collect(),
      None => HashMap::new(),
    };

    vars.set_value(map);
    vars
  }
}

impl Into<Context> for Vars {
  fn into(self) -> Context {
    Context::from(self.context)
  }
}

impl Serialize for Vars {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    // Clone the context first, then convert to json
    let context = self.context.clone();
    let map = context.into_json();
    map.serialize(serializer)
  }
}

impl<'de> Deserialize<'de> for Vars {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let map = HashMap::<String, Value>::deserialize(deserializer)?;
    let mut vars = Vars::new();
    vars.set_value(Some(map));

    Ok(vars)
  }
}

pub struct VarsIter {
  map: HashMap<String, Value>,
  keys: Vec<String>,
  current: usize,
}

impl Iterator for VarsIter {
  type Item = (String, Value);

  fn next(&mut self) -> Option<Self::Item> {
    if self.current >= self.keys.len() {
      return None;
    }

    let key = &self.keys[self.current];
    self.current += 1;

    self.map.get(key).map(|value| (key.clone(), value.clone()))
  }
}

impl IntoIterator for Vars {
  type Item = (String, Value);
  type IntoIter = VarsIter;

  fn into_iter(self) -> Self::IntoIter {
    self.iter()
  }
}
