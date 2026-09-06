use std::{
  collections::{HashMap, HashSet},
  env,
  fmt::{Debug, Display, Formatter},
  path::PathBuf,
  sync::Arc,
};

use async_trait::async_trait;
use indexmap::IndexMap;
use lazy_static::lazy_static;
use octa_octafile::{RequiredMode, VariableEnum, VariableSource, Vars as OctafileVars};
use octa_plugin_manager::plugin_manager::PluginManager;
use regex::Regex;
use serde::Serialize;
use tera::{Context, Value};
use tokio_util::sync::CancellationToken;
use tracing::debug;

use crate::{
  error::{ExecutorError, ExecutorResult},
  plugin::{ManagerPluginEvaluator, PluginEvaluator, PluginExecutionContext, PluginTarget},
  template::{PluginTemplateContext, TemplateRenderer},
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
  required_vars: IndexMap<String, RequiredVar>,
  parent: Option<Arc<Vars>>, // Link to parent variables
  dir: Option<PathBuf>,      // Directory used by shell-backed values in this context
  expanded: bool,            // Indicates that all inherited values have been expanded
}

/// One inherited variable scope together with its execution context.
struct VariableLayer {
  values: IndexMap<String, Value>,
  secrets: HashSet<String>,
  required_vars: IndexMap<String, RequiredVar>,
  dir: Option<PathBuf>,
}

/// Requirement metadata retained until its enum templates can see all variable layers.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RequiredVar {
  mode: RequiredMode,
  secret: bool,
  enum_source: Option<VariableEnum>,
  question: Option<String>,
}

/// Requirement with concrete choices ready for validation or interactive input.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ResolvedRequiredVar {
  mode: RequiredMode,
  secret: bool,
  enum_values: Option<Vec<String>>,
  question: Option<String>,
}

struct SuppliedRequiredVar {
  value: Value,
  secret: bool,
}

struct ResolvedVars {
  values: IndexMap<String, Value>,
  secrets: HashSet<String>,
  required_vars: IndexMap<String, RequiredVar>,
}

/// Describes one missing variable that may be requested from an external input provider.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct VariablePrompt {
  pub name: String,
  pub question: String,
  pub enum_values: Option<Vec<String>>,
  pub secret: bool,
}

/// Resolves interactive input without coupling the executor to a terminal implementation.
#[async_trait]
pub trait VariableResolver: Send + Sync {
  async fn resolve(&self, prompt: &VariablePrompt) -> Result<String, String>;
}

impl PartialEq for Vars {
  fn eq(&self, other: &Self) -> bool {
    self.values == other.values && self.secrets == other.secrets && self.required_vars == other.required_vars
  }
}

impl Eq for Vars {}

impl Vars {
  pub fn new() -> Self {
    Self {
      values: IndexMap::new(),
      secrets: HashSet::new(),
      required_vars: IndexMap::new(),
      parent: None,
      dir: None,
      expanded: false,
    }
  }

  pub fn with_parent(parent: Vars) -> Self {
    Self {
      values: IndexMap::new(),
      secrets: HashSet::new(),
      required_vars: IndexMap::new(),
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
    self.required_vars.clear();
    self.expanded = false;
  }

  pub(crate) fn set_variables(&mut self, variables: OctafileVars) {
    self.values.clear();
    self.secrets.clear();
    self.required_vars.clear();
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
      let enum_source = variable.enum_source().cloned();
      let question = variable.question().map(str::to_owned);
      let value = match variable.into_source() {
        VariableSource::Value(value) => value,
        VariableSource::Shell(command) => serde_json::json!({ "sh": command }),
        VariableSource::Required(mode) => {
          self.required_vars.insert(
            key,
            RequiredVar {
              mode,
              secret,
              enum_source,
              question,
            },
          );
          continue;
        },
      };

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

  /// Resolves and validates requirements when an executable node reaches runtime.
  pub(crate) async fn resolve_required(&mut self, resolver: Option<&dyn VariableResolver>) -> ExecutorResult<()> {
    let contexts = self.collect_context_chain();
    let required_vars = resolve_required_vars(&contexts, collect_required_vars(&contexts))?;
    let supplied = collect_supplied_required_vars(&contexts, &required_vars);
    let mut prompts = Vec::new();

    // Validate the complete non-interactive configuration before asking the user for anything.
    for (name, required) in &required_vars {
      if let Some(value) = supplied.get(name).filter(|value| !is_empty_value(&value.value)) {
        validate_required_value(name, required.enum_values.as_deref(), &value.value)?;
        continue;
      }

      if required.mode == RequiredMode::Strict {
        return Err(ExecutorError::RequiredVariableMissing(name.clone()));
      }

      prompts.push((name.clone(), required.clone()));
    }

    for (name, required) in prompts {
      let resolver = resolver.ok_or_else(|| ExecutorError::VariablePromptUnavailable(name.clone()))?;
      let question = required.question.unwrap_or_else(|| {
        if required.enum_values.is_some() {
          format!("Select a value for '{name}'")
        } else {
          format!("Enter a value for '{name}'")
        }
      });
      let prompt = VariablePrompt {
        name: name.clone(),
        question,
        enum_values: required.enum_values.clone(),
        secret: required.secret,
      };
      let value = Value::String(
        resolver
          .resolve(&prompt)
          .await
          .map_err(|message| ExecutorError::VariablePromptFailed(name.clone(), message))?,
      );
      validate_required_value(&name, required.enum_values.as_deref(), &value)?;

      self.values.insert(name.clone(), value);
      if required.secret {
        self.secrets.insert(name);
      }
    }

    Ok(())
  }

  pub async fn expand(&mut self, dry: bool) -> ExecutorResult<()> {
    self
      .expand_with_evaluator_option(None, dry, CancellationToken::new(), IndexMap::new())
      .await
  }

  /// Expands variables whose templates or `sh` sources invoke registered plugins.
  pub async fn expand_with_plugins(&mut self, manager: Arc<PluginManager>, dry: bool) -> ExecutorResult<()> {
    self
      .expand_with_evaluator(
        Arc::new(ManagerPluginEvaluator::new(manager)),
        dry,
        CancellationToken::new(),
      )
      .await
  }

  pub(crate) async fn expand_with_evaluator(
    &mut self,
    evaluator: Arc<dyn PluginEvaluator>,
    dry: bool,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<()> {
    self
      .expand_with_evaluator_option(Some(evaluator), dry, cancel_token, IndexMap::new())
      .await
  }

  /// Expands variables with runtime values that are visible to every inherited layer and win
  /// over configured values with the same name.
  pub(crate) async fn expand_with_evaluator_and_overrides(
    &mut self,
    evaluator: Arc<dyn PluginEvaluator>,
    dry: bool,
    cancel_token: CancellationToken,
    overrides: IndexMap<String, Value>,
  ) -> ExecutorResult<()> {
    self
      .expand_with_evaluator_option(Some(evaluator), dry, cancel_token, overrides)
      .await
  }

  async fn expand_with_evaluator_option(
    &mut self,
    evaluator: Option<Arc<dyn PluginEvaluator>>,
    dry: bool,
    cancel_token: CancellationToken,
    overrides: IndexMap<String, Value>,
  ) -> ExecutorResult<()> {
    if self.expanded {
      for (key, value) in overrides {
        self.values.insert(key.clone(), value);
        self.secrets.remove(&key);
      }
      return Ok(());
    }

    let contexts = self.collect_context_chain();
    let resolved = self
      .process_context_chain(contexts, evaluator, dry, cancel_token, overrides)
      .await?;
    self.values = resolved.values;
    self.secrets = resolved.secrets;
    self.required_vars = resolved.required_vars;
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
        required_vars: vars.required_vars.clone(),
        dir: vars.dir.clone(),
      });
      current = vars.parent.as_ref().map(|p| p.as_ref());
    }

    contexts.into_iter().rev().collect()
  }

  async fn process_context_chain(
    &self,
    contexts: Vec<VariableLayer>,
    evaluator: Option<Arc<dyn PluginEvaluator>>,
    dry: bool,
    cancel_token: CancellationToken,
    overrides: IndexMap<String, Value>,
  ) -> ExecutorResult<ResolvedVars> {
    let required_definitions = collect_required_vars(&contexts);
    let required_vars = resolve_required_vars(&contexts, required_definitions.clone())?;
    let supplied_required_vars = collect_supplied_required_vars(&contexts, &required_vars);
    validate_required_vars(
      &required_vars,
      supplied_required_vars
        .iter()
        .map(|(key, supplied)| (key, &supplied.value)),
    )?;
    let mut accumulated = supplied_required_vars
      .iter()
      .map(|(key, supplied)| (key.clone(), supplied.value.clone()))
      .collect::<IndexMap<_, _>>();
    let mut secrets = supplied_required_vars
      .iter()
      .filter(|(key, supplied)| supplied.secret || required_vars.get(*key).is_some_and(|required| required.secret))
      .map(|(key, _)| key.clone())
      .collect::<HashSet<_>>();
    for (key, value) in &overrides {
      accumulated.insert(key.clone(), value.clone());
      secrets.remove(key);
    }

    // Each value is added immediately after expansion. This preserves YAML declaration order
    // and makes earlier values in the same `vars` mapping available to later ones.
    for layer in contexts {
      let VariableLayer {
        values,
        secrets: layer_secrets,
        required_vars: _,
        dir,
      } = layer;
      let current_dir = match dir {
        Some(dir) => dir,
        None => env::current_dir()?,
      };

      for (key, value) in values {
        if overrides.contains_key(&key) {
          continue;
        }
        // Required values are concrete inputs and were inserted before dependent declarations.
        if required_vars.contains_key(&key) {
          continue;
        }
        let secret = layer_secrets.contains(&key);

        // Literal values need neither a Tera instance nor a plugin execution context.
        if shell_command(&value).is_none() && !value_contains_template(&value) {
          accumulated.insert(key.clone(), value);
          if secret {
            secrets.insert(key);
          } else {
            secrets.remove(&key);
          }
          continue;
        }

        let template_context = Context::from_serialize(&accumulated).map_err(|error| {
          ExecutorError::VariableExpandError(key.clone(), format!("failed to build template context: {error}"))
        })?;
        let environment = values_as_environment(&accumulated);
        let plugin_context = PluginTemplateContext::new(
          evaluator.clone(),
          PluginExecutionContext {
            dir: current_dir.clone(),
            vars: accumulated
              .iter()
              .map(|(key, value)| (key.clone(), value.clone()))
              .collect(),
            envs: environment,
            secret_vars: secrets.iter().cloned().collect(),
            dry,
            redact_params: secret,
          },
          cancel_token.clone(),
        );
        let renderer = TemplateRenderer::new(template_context, plugin_context);
        let processed = self.process_template_value(&key, &value, &renderer, secret).await?;

        accumulated.insert(key.clone(), processed);
        if secret {
          secrets.insert(key);
        } else {
          secrets.remove(&key);
        }
      }
    }

    Ok(ResolvedVars {
      values: accumulated,
      secrets,
      required_vars: required_definitions,
    })
  }

  async fn process_template_value(
    &self,
    key: &str,
    value: &Value,
    renderer: &TemplateRenderer,
    secret: bool,
  ) -> ExecutorResult<Value> {
    if let Some(command) = shell_command(value) {
      let command = renderer
        .render(command)
        .await
        .map_err(|error| variable_error(key, error, secret))?;
      if secret {
        debug!("Processing secret shell-backed variable '{}'", key);
      } else {
        debug!("Processing shell-backed variable '{}': '{}'", key, command);
      }
      return renderer
        .evaluate(
          PluginTarget::Capability(octa_plugin::SHELL_CAPABILITY.to_owned()),
          Value::String(command),
        )
        .await
        .map(Value::String)
        .map_err(|error| variable_error(key, error.to_string(), secret));
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
    let res = renderer
      .render(val)
      .await
      .map_err(|error| variable_error(key, error, secret))?;
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

  /// Names explicitly declared by Octa configuration or invocation layers.
  pub(crate) fn declared_names(&self) -> HashSet<String> {
    self
      .collect_context_chain()
      .into_iter()
      .flat_map(|layer| layer.values.into_keys().chain(layer.required_vars.into_keys()))
      .collect()
  }

  /// Names of values that plugins must redact from their diagnostic logs.
  pub(crate) fn secret_names(&self) -> Vec<String> {
    self.secrets.iter().cloned().collect()
  }
}

fn collect_required_vars(contexts: &[VariableLayer]) -> IndexMap<String, RequiredVar> {
  let mut required_vars: IndexMap<String, RequiredVar> = IndexMap::new();
  for layer in contexts {
    for (key, required) in &layer.required_vars {
      let mut required = required.clone();
      if let Some(previous) = required_vars.get(key) {
        required.secret |= previous.secret;
      }
      required_vars.insert(key.clone(), required);
    }
  }
  required_vars
}

fn resolve_required_vars(
  contexts: &[VariableLayer],
  required_vars: IndexMap<String, RequiredVar>,
) -> ExecutorResult<IndexMap<String, ResolvedRequiredVar>> {
  let (values, mut secrets) = collect_enum_context(contexts);
  secrets.extend(
    required_vars
      .iter()
      .filter(|(_, required)| required.secret)
      .map(|(name, _)| name.clone()),
  );
  required_vars
    .into_iter()
    .map(|(name, required)| {
      let enum_values = required
        .enum_source
        .as_ref()
        .map(|source| crate::variable_enum::resolve(&name, source, &values, &secrets))
        .transpose()?;
      Ok((
        name,
        ResolvedRequiredVar {
          mode: required.mode,
          secret: required.secret,
          enum_values,
          question: required.question,
        },
      ))
    })
    .collect()
}

fn collect_enum_context(contexts: &[VariableLayer]) -> (IndexMap<String, Value>, HashSet<String>) {
  let mut values = IndexMap::new();
  let mut secrets = HashSet::new();

  for layer in contexts {
    for (name, value) in &layer.values {
      values.insert(name.clone(), value.clone());
      if layer.secrets.contains(name) {
        secrets.insert(name.clone());
      } else {
        secrets.remove(name);
      }
    }
  }

  (values, secrets)
}

fn collect_supplied_required_vars(
  contexts: &[VariableLayer],
  required_vars: &IndexMap<String, ResolvedRequiredVar>,
) -> IndexMap<String, SuppliedRequiredVar> {
  let mut supplied = IndexMap::new();
  for layer in contexts {
    for (key, value) in &layer.values {
      if required_vars.contains_key(key) {
        supplied.insert(
          key.clone(),
          SuppliedRequiredVar {
            value: value.clone(),
            secret: layer.secrets.contains(key),
          },
        );
      }
    }
  }

  supplied
}

fn validate_required_vars<'a>(
  required_vars: &IndexMap<String, ResolvedRequiredVar>,
  values: impl Iterator<Item = (&'a String, &'a Value)>,
) -> ExecutorResult<()> {
  let mut supplied = HashMap::new();
  for (key, value) in values {
    supplied.insert(key, value);
  }

  for key in required_vars.keys() {
    let Some(value) = supplied.get(key).filter(|value| !is_empty_value(value)) else {
      return Err(ExecutorError::RequiredVariableMissing(key.clone()));
    };
    validate_required_value(key, required_vars[key].enum_values.as_deref(), value)?;
  }

  Ok(())
}

fn validate_required_value(name: &str, enum_values: Option<&[String]>, value: &Value) -> ExecutorResult<()> {
  if is_empty_value(value) {
    return Err(ExecutorError::RequiredVariableMissing(name.to_owned()));
  }
  if shell_command(value).is_some() || value_contains_template(value) {
    return Err(ExecutorError::RequiredVariableNotConcrete(name.to_owned()));
  }
  let Some(enum_values) = enum_values else {
    return Ok(());
  };
  let valid = value
    .as_str()
    .is_some_and(|value| enum_values.iter().any(|allowed| allowed == value));
  if !valid {
    return Err(ExecutorError::RequiredVariableNotAllowed(
      name.to_owned(),
      enum_values.join(", "),
    ));
  }

  Ok(())
}

fn shell_command(value: &Value) -> Option<&str> {
  let object = value.as_object()?;
  (object.len() == 1).then(|| object.get("sh")?.as_str()).flatten()
}

fn is_empty_value(value: &Value) -> bool {
  match value {
    Value::Null => true,
    Value::String(value) => value.trim().is_empty(),
    Value::Array(values) => values.is_empty(),
    Value::Object(values) => values.is_empty(),
    Value::Bool(_) | Value::Number(_) => false,
  }
}

fn value_contains_template(value: &Value) -> bool {
  match value {
    Value::String(value) => TEMPLATE_REGEX.is_match(value),
    Value::Array(values) => values.iter().any(value_contains_template),
    Value::Object(values) => values.values().any(value_contains_template),
    Value::Null | Value::Bool(_) | Value::Number(_) => false,
  }
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
      .field("required_vars", &self.required_vars.keys().collect::<Vec<_>>())
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
      required_vars: IndexMap::new(),
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
  use std::{fs, sync::Mutex};

  use super::*;
  use serde_json::json;
  use tempfile::TempDir;

  use crate::plugin::SystemTestEvaluator;

  async fn expand_with_system(vars: &mut Vars) -> ExecutorResult<()> {
    vars
      .expand_with_evaluator(Arc::new(SystemTestEvaluator), false, CancellationToken::new())
      .await
  }

  struct FixedResolver {
    value: String,
    prompts: Mutex<Vec<VariablePrompt>>,
  }

  impl FixedResolver {
    fn new(value: &str) -> Self {
      Self {
        value: value.to_owned(),
        prompts: Mutex::new(Vec::new()),
      }
    }
  }

  #[async_trait]
  impl VariableResolver for FixedResolver {
    async fn resolve(&self, prompt: &VariablePrompt) -> Result<String, String> {
      self.prompts.lock().unwrap().push(prompt.clone());
      Ok(self.value.clone())
    }
  }

  #[test]
  fn test_new_vars() {
    let vars = Vars::new();
    assert!(vars.parent.is_none());
    assert!(!vars.expanded);
    assert!(vars.values.is_empty());
  }

  #[tokio::test]
  async fn re_expansion_applies_runtime_overrides_and_removes_secret_metadata() {
    let mut vars = Vars::with_value(json!({"value": "initial"}));
    vars
      .expand_with_evaluator_option(None, false, CancellationToken::new(), IndexMap::new())
      .await
      .unwrap();
    vars.secrets.insert("value".to_owned());

    vars
      .expand_with_evaluator_option(
        None,
        false,
        CancellationToken::new(),
        IndexMap::from([("value".to_owned(), json!("override"))]),
      )
      .await
      .unwrap();

    assert_eq!(vars.get("value"), Some(&json!("override")));
    assert!(!vars.secrets.contains("value"));
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
  async fn expands_templates_nested_in_structured_values() {
    let values: octa_octafile::Vars = serde_yml::from_str(
      r#"
      NAME: Octa
      CONFIG:
        nested: "Hello {{ NAME }}"
      "#,
    )
    .unwrap();
    let mut vars = Vars::with_variables(values);

    vars.expand(true).await.unwrap();

    assert_eq!(vars.get("CONFIG"), Some(&json!({ "nested": "Hello Octa" })));
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
  async fn validates_required_variables_after_merging_layers() {
    let required: octa_octafile::Vars = serde_yml::from_str(
      r#"
      API_TOKEN:
        required: true
        secret: true
      "#,
    )
    .unwrap();
    let parent = Vars::with_variables(required);
    let mut vars = Vars::with_value_and_parent(json!({ "API_TOKEN": "external-token" }), parent);

    vars.expand(true).await.unwrap();

    assert_eq!(vars.get("API_TOKEN"), Some(&Value::String("external-token".to_owned())));
    assert_eq!(vars.secret_names(), vec!["API_TOKEN"]);
    assert!(!format!("{vars}").contains("external-token"));
  }

  #[tokio::test]
  async fn resolves_prompted_variables_and_preserves_metadata() {
    let configured: octa_octafile::Vars = serde_yml::from_str(
      r#"
      ENVIRONMENT:
        required: prompt
        secret: true
        question: Choose environment
        enum: [development, production]
      MESSAGE: "Deploying to {{ ENVIRONMENT }}"
      "#,
    )
    .unwrap();
    let resolver = FixedResolver::new("production");
    let mut vars = Vars::with_variables(configured);

    vars.resolve_required(Some(&resolver)).await.unwrap();
    vars.expand(true).await.unwrap();

    assert_eq!(vars.get("ENVIRONMENT"), Some(&json!("production")));
    assert_eq!(vars.get("MESSAGE"), Some(&json!("Deploying to production")));
    assert_eq!(vars.secret_names(), vec!["ENVIRONMENT"]);
    assert_eq!(
      *resolver.prompts.lock().unwrap(),
      vec![VariablePrompt {
        name: "ENVIRONMENT".to_owned(),
        question: "Choose environment".to_owned(),
        enum_values: Some(vec!["development".to_owned(), "production".to_owned()]),
        secret: true,
      }]
    );
  }

  #[tokio::test]
  async fn resolves_enum_from_another_variable() {
    let configured: octa_octafile::Vars = serde_yml::from_str(
      r#"
      ENVIRONMENTS: [development, production]
      ENVIRONMENT:
        required: prompt
        enum: "{{ ENVIRONMENTS }}"
      "#,
    )
    .unwrap();
    let resolver = FixedResolver::new("production");
    let mut vars = Vars::with_variables(configured);

    vars.resolve_required(Some(&resolver)).await.unwrap();

    assert_eq!(vars.get("ENVIRONMENT"), Some(&json!("production")));
    assert_eq!(
      resolver.prompts.lock().unwrap()[0].enum_values,
      Some(vec!["development".to_owned(), "production".to_owned()])
    );
  }

  #[tokio::test]
  async fn expands_templates_in_literal_enum_options() {
    let configured: octa_octafile::Vars = serde_yml::from_str(
      r#"
      DEFAULT_ENVIRONMENT: development
      ENVIRONMENT:
        required: prompt
        enum: ["{{ DEFAULT_ENVIRONMENT }}", production]
      "#,
    )
    .unwrap();
    let resolver = FixedResolver::new("development");
    let mut vars = Vars::with_variables(configured);

    vars.resolve_required(Some(&resolver)).await.unwrap();

    assert_eq!(
      resolver.prompts.lock().unwrap()[0].enum_values,
      Some(vec!["development".to_owned(), "production".to_owned()])
    );
  }

  #[tokio::test]
  async fn rejects_enum_references_that_are_not_string_lists() {
    let configured: octa_octafile::Vars = serde_yml::from_str(
      r#"
      ENVIRONMENTS: development
      ENVIRONMENT:
        required: prompt
        enum: "{{ ENVIRONMENTS }}"
      "#,
    )
    .unwrap();
    let mut vars = Vars::with_variables(configured);

    assert!(matches!(
      vars.resolve_required(Some(&FixedResolver::new("development"))).await,
      Err(ExecutorError::RequiredVariableEnumError(name, message))
        if name == "ENVIRONMENT" && message == "value must be a list of strings"
    ));
  }

  #[tokio::test]
  async fn generates_default_questions_and_rejects_unavailable_input() {
    let configured: octa_octafile::Vars =
      serde_yml::from_str("ENVIRONMENT:\n  required: prompt\n  enum: [development, production]\n").unwrap();
    let resolver = FixedResolver::new("development");
    let mut vars = Vars::with_variables(configured.clone());

    vars.resolve_required(Some(&resolver)).await.unwrap();
    assert_eq!(
      resolver.prompts.lock().unwrap()[0].question,
      "Select a value for 'ENVIRONMENT'"
    );

    let mut vars = Vars::with_variables(configured);
    assert!(matches!(
      vars.resolve_required(None).await,
      Err(ExecutorError::VariablePromptUnavailable(name)) if name == "ENVIRONMENT"
    ));
  }

  #[tokio::test]
  async fn generates_default_text_questions_and_rejects_empty_answers() {
    let configured: octa_octafile::Vars = serde_yml::from_str("TOKEN:\n  required: prompt\n").unwrap();
    let resolver = FixedResolver::new("");
    let mut vars = Vars::with_variables(configured);

    assert!(matches!(
      vars.resolve_required(Some(&resolver)).await,
      Err(ExecutorError::RequiredVariableMissing(name)) if name == "TOKEN"
    ));
    assert_eq!(
      resolver.prompts.lock().unwrap()[0].question,
      "Enter a value for 'TOKEN'"
    );
  }

  #[tokio::test]
  async fn inherited_secret_requirement_cannot_be_downgraded() {
    let parent: octa_octafile::Vars = serde_yml::from_str("TOKEN:\n  required: prompt\n  secret: true\n").unwrap();
    let child: octa_octafile::Vars = serde_yml::from_str("TOKEN:\n  required: prompt\n").unwrap();
    let resolver = FixedResolver::new("hidden");
    let parent = Vars::with_variables(parent);
    let mut vars = Vars::with_variables_and_parent(child, parent);

    vars.resolve_required(Some(&resolver)).await.unwrap();

    assert!(resolver.prompts.lock().unwrap()[0].secret);
    assert_eq!(vars.secret_names(), vec!["TOKEN"]);
  }

  #[tokio::test]
  async fn validates_enum_values_from_non_interactive_sources() {
    let required: octa_octafile::Vars =
      serde_yml::from_str("ENVIRONMENT:\n  required: true\n  enum: [development, production]\n").unwrap();
    let parent = Vars::with_variables(required);
    let mut vars = Vars::with_value_and_parent(json!({ "ENVIRONMENT": "testing" }), parent);

    assert!(matches!(
      vars.resolve_required(None).await,
      Err(ExecutorError::RequiredVariableNotAllowed(name, allowed))
        if name == "ENVIRONMENT" && allowed == "development, production"
    ));
  }

  #[tokio::test]
  async fn validates_strict_requirements_before_prompting() {
    let configured: octa_octafile::Vars = serde_yml::from_str(
      r#"
      ENVIRONMENT:
        required: prompt
      TOKEN:
        required: true
      "#,
    )
    .unwrap();
    let resolver = FixedResolver::new("production");
    let mut vars = Vars::with_variables(configured);

    assert!(matches!(
      vars.resolve_required(Some(&resolver)).await,
      Err(ExecutorError::RequiredVariableMissing(name)) if name == "TOKEN"
    ));
    assert!(resolver.prompts.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn child_requirement_accepts_an_inherited_value() {
    let required: octa_octafile::Vars = serde_yml::from_str("PROFILE:\n  required: true\n").unwrap();
    let parent = Vars::with_value(json!({ "PROFILE": "production" }));
    let mut vars = Vars::with_variables_and_parent(required, parent);

    vars.expand(true).await.unwrap();

    assert_eq!(vars.get("PROFILE"), Some(&Value::String("production".to_owned())));
  }

  #[tokio::test]
  async fn required_values_are_available_to_configured_templates() {
    let configured: octa_octafile::Vars = serde_yml::from_str(
      r#"
      TOKEN:
        required: true
      HEADER: "Bearer {{ TOKEN }}"
      "#,
    )
    .unwrap();
    let parent = Vars::with_variables(configured);
    let mut vars = Vars::with_value_and_parent(json!({ "TOKEN": "external-token" }), parent);

    vars.expand(true).await.unwrap();

    assert_eq!(
      vars.get("HEADER"),
      Some(&Value::String("Bearer external-token".to_owned()))
    );
  }

  #[tokio::test]
  async fn required_values_must_be_concrete() {
    let required: octa_octafile::Vars = serde_yml::from_str("TOKEN:\n  required: true\n").unwrap();

    for value in [json!("{{ PREFIX }}"), json!({ "sh": "echo token" })] {
      let parent = Vars::with_variables(required.clone());
      let mut vars = Vars::with_value_and_parent(json!({ "TOKEN": value }), parent);
      assert!(matches!(
        vars.expand(true).await,
        Err(ExecutorError::RequiredVariableNotConcrete(name)) if name == "TOKEN"
      ));
    }
  }

  #[tokio::test]
  async fn rejects_missing_and_empty_required_values() {
    let required: octa_octafile::Vars = serde_yml::from_str("VALUE:\n  required: true\n").unwrap();
    let mut missing = Vars::with_variables(required.clone());
    assert!(matches!(
      missing.expand(true).await,
      Err(ExecutorError::RequiredVariableMissing(name)) if name == "VALUE"
    ));

    for value in [Value::Null, json!("  "), json!([]), json!({})] {
      let parent = Vars::with_variables(required.clone());
      let mut vars = Vars::with_value_and_parent(json!({ "VALUE": value }), parent);
      assert!(matches!(
        vars.expand(true).await,
        Err(ExecutorError::RequiredVariableMissing(name)) if name == "VALUE"
      ));
    }
  }

  #[tokio::test]
  async fn validates_required_variables_before_running_shell_values() {
    let temp_dir = TempDir::new().unwrap();
    let marker = temp_dir.path().join("executed");
    #[cfg(windows)]
    let command = "echo executed>executed";
    #[cfg(not(windows))]
    let command = "touch executed";
    let values: octa_octafile::Vars = serde_yml::from_str(&format!(
      "SIDE_EFFECT:\n  sh: '{command}'\nREQUIRED:\n  required: true\n"
    ))
    .unwrap();
    let mut vars = Vars::with_variables(values);
    vars.set_dir(temp_dir.path());

    assert!(matches!(
      vars.expand(false).await,
      Err(ExecutorError::RequiredVariableMissing(name)) if name == "REQUIRED"
    ));
    assert!(!marker.exists());
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

    expand_with_system(&mut vars).await.unwrap();

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

    let error = expand_with_system(&mut vars).await.unwrap_err().to_string();

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

    let error = expand_with_system(&mut vars).await.unwrap_err().to_string();

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

    expand_with_system(&mut vars).await.unwrap();

    assert_eq!(vars.get("STRUCTURED"), Some(&Value::String("dynamic".to_owned())));
    assert_eq!(vars.get("FILTERED"), Some(&Value::String("prefix-dynamic".to_owned())));
  }
}
