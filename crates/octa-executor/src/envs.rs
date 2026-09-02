use std::{
  borrow::Cow,
  collections::HashMap,
  env,
  fmt::{Debug, Display, Formatter},
  path::PathBuf,
  sync::Arc,
};

use octa_octafile::EnvValue;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tera::{Context, Tera};
use tracing::debug;

use crate::{
  error::{ExecutorError, ExecutorResult},
  function::{format_tera_error, register_shell_with_redactions, ExecuteShell},
  vars::Vars,
};

type EnvContext = HashMap<String, EnvValue>;
type ResolvedEnvContext = HashMap<String, String>;

#[derive(Clone, Default)]
pub struct Envs {
  context: EnvContext,       // Current environments
  parent: Option<Arc<Envs>>, // Link to parent environments
  dir: Option<PathBuf>,      // Directory used by shell-backed values in this context
  expanded: bool,            // Indicates that all inherited values have been expanded
}

// Environment values may contain secrets rendered from task variables. Debug output therefore
// exposes only structural information, even though Envs itself does not own sensitivity metadata.
impl Debug for Envs {
  fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
    let mut keys = self.context.keys().collect::<Vec<_>>();
    keys.sort_unstable();
    formatter
      .debug_struct("Envs")
      .field("keys", &keys)
      .field("parent", &self.parent)
      .field("dir", &self.dir)
      .field("expanded", &self.expanded)
      .finish()
  }
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
      dir: None,
      expanded: false,
    }
  }

  pub fn with_parent(parent: Envs) -> Self {
    Self {
      context: HashMap::default(),
      parent: Some(Arc::new(parent)),
      dir: None,
      expanded: false,
    }
  }

  pub fn with_value<V: Into<EnvValue>>(value: HashMap<String, V>) -> Self {
    let mut envs = Self::default();
    envs.set_value(value);
    envs
  }

  pub fn with_value_and_parent<V: Into<EnvValue>>(value: HashMap<String, V>, parent: Envs) -> Self {
    let mut envs = Self::with_parent(parent);
    envs.set_value(value);
    envs
  }

  pub fn set_parent(&mut self, parent: Option<Envs>) {
    self.parent = parent.map(Arc::new);
    self.expanded = false;
  }

  /// Sets the working directory for `shell()` calls declared in this environment layer.
  pub fn set_dir(&mut self, dir: impl Into<PathBuf>) {
    self.dir = Some(dir.into());
    self.expanded = false;
  }

  pub fn set_value<V: Into<EnvValue>>(&mut self, value: HashMap<String, V>) {
    self.context = value.into_iter().map(|(key, value)| (key, value.into())).collect();
    self.expanded = false;
  }

  pub fn get(&self, key: &str) -> Option<&String> {
    match self.context.get(key) {
      Some(EnvValue::String(value)) => Some(value),
      _ => None,
    }
  }

  pub fn insert<T: AsRef<str>>(&mut self, key: &T, value: &T) {
    self
      .context
      .insert(key.as_ref().to_string(), EnvValue::String(value.as_ref().to_string()));
    self.expanded = false;
  }

  pub fn extend<V: Into<EnvValue>>(&mut self, source: HashMap<String, V>) {
    self
      .context
      .extend(source.into_iter().map(|(key, value)| (key, value.into())));
    self.expanded = false;
  }

  pub fn iter(&self) -> EnvsIter {
    EnvsIter::new(self.context.clone())
  }

  pub(crate) fn to_merged_hashmap(&self) -> HashMap<String, EnvValue> {
    let mut result = HashMap::new();
    for (context, _) in self.collect_context_chain() {
      result.extend(context);
    }
    result
  }

  pub async fn expand(&mut self) -> ExecutorResult<()> {
    self.expand_with(&Vars::new(), false)
  }

  /// Expands the complete environment hierarchy using the resolved task variables.
  pub fn expand_with(&mut self, vars: &Vars, dry: bool) -> ExecutorResult<()> {
    if self.expanded {
      return Ok(());
    }

    let contexts = self.collect_context_chain();
    let processed_context = self.process_context_chain(contexts, vars, dry)?;
    self.context = processed_context;
    self.expanded = true;

    Ok(())
  }

  fn collect_context_chain(&self) -> Vec<(EnvContext, Option<PathBuf>)> {
    let mut contexts = Vec::new();
    let mut current = Some(self);

    while let Some(envs) = current {
      contexts.push((envs.context.clone(), envs.dir.clone()));
      current = envs.parent.as_ref().map(|p| p.as_ref());
    }

    contexts.into_iter().rev().collect()
  }

  fn process_context_chain(
    &self,
    contexts: Vec<(EnvContext, Option<PathBuf>)>,
    vars: &Vars,
    dry: bool,
  ) -> ExecutorResult<EnvContext> {
    let mut accumulated = EnvContext::new();

    // Parent layers are processed first so every child layer can override and reference them.
    for (context, dir) in contexts {
      let current_dir = match dir {
        Some(dir) => dir,
        None => env::current_dir()?,
      };
      let processed = self.process_single_context(context, &accumulated, vars, &current_dir, dry)?;
      accumulated.extend(processed);
    }

    Ok(accumulated)
  }

  fn process_single_context(
    &self,
    context: EnvContext,
    parent: &EnvContext,
    vars: &Vars,
    current_dir: &std::path::Path,
    dry: bool,
  ) -> ExecutorResult<EnvContext> {
    let mut processed = EnvContext::new();
    let parent = resolved_environment(parent);

    // Resolve literal values first so templates and shell commands can consume dotenv and
    // ordinary environment values declared at the same level without depending on map order.
    for (key, value) in &context {
      if let EnvValue::String(value) = value {
        if !is_template(value) {
          processed.insert(key.clone(), EnvValue::String(expand_environment_value(value, &parent)));
        }
      }
    }

    let mut available = parent.clone();
    available.extend(resolved_environment(&processed));

    // Template values receive inherited values and same-layer literals. Other same-layer
    // templates are deliberately excluded because HashMap declaration order is not stable.
    let mut tera = Tera::default();
    let shell = register_shell_with_redactions(
      &mut tera,
      current_dir,
      available.clone(),
      dry,
      vars.secret_redactions(),
      false,
    );
    let mut template_context: Context = vars.clone().into();
    template_context.extend(
      Context::from_serialize(&available)
        .map_err(|error| ExecutorError::ValueExpandError("environment context".to_string(), error.to_string()))?,
    );

    for (key, value) in context {
      let processed_value = match value {
        EnvValue::String(value) if is_template(&value) => {
          Some(self.process_template_value(&key, &value, &available, &mut tera, &template_context)?)
        },
        EnvValue::Shell(shell_value) => {
          Some(self.process_shell_value(&key, &shell_value.sh, &available, &mut tera, &template_context, &shell)?)
        },
        EnvValue::String(_) => None,
      };

      if let Some(value) = processed_value {
        processed.insert(key, EnvValue::String(value));
      }
    }

    Ok(processed)
  }

  fn process_template_value(
    &self,
    key: &str,
    value: &str,
    context: &ResolvedEnvContext,
    tera: &mut Tera,
    template_context: &Context,
  ) -> ExecutorResult<String> {
    let mut val = value.trim().to_owned();

    val = tera
      .render_str(&val, template_context)
      .map_err(|error| ExecutorError::ValueExpandError(value.to_string(), format_tera_error(&error)))?;

    debug!("Processing template environment '{}'", key);
    Ok(expand_environment_value(&val, context))
  }

  fn process_shell_value(
    &self,
    key: &str,
    command: &str,
    context: &ResolvedEnvContext,
    tera: &mut Tera,
    template_context: &Context,
    shell: &ExecuteShell,
  ) -> ExecutorResult<String> {
    let command = tera
      .render_str(command, template_context)
      .map_err(|error| ExecutorError::ValueExpandError(key.to_owned(), format_tera_error(&error)))?;
    debug!("Processing shell-backed environment '{}'", key);

    let value = shell
      .execute(&command)
      .map_err(|error| ExecutorError::ValueExpandError(key.to_owned(), format_tera_error(&error)))?;
    Ok(expand_environment_value(&value, context))
  }
}

fn is_template(value: &str) -> bool {
  value.contains("{{") && value.contains("}}")
}

fn resolved_environment(context: &EnvContext) -> ResolvedEnvContext {
  context
    .iter()
    .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned())))
    .collect()
}

fn expand_environment_value(value: &str, context: &ResolvedEnvContext) -> String {
  let get_env = |name: &str| match context.get(name) {
    Some(value) => Some(Cow::Borrowed(value.as_str())),
    None => env::var(name).map(Cow::Owned).ok(),
  };

  shellexpand::env_with_context_no_errors(value.trim(), get_env).into_owned()
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
      dir: None,
      expanded: false,
    }
  }
}

impl From<ResolvedEnvContext> for Envs {
  fn from(context: ResolvedEnvContext) -> Self {
    Self::with_value(context)
  }
}

impl From<Envs> for ResolvedEnvContext {
  fn from(envs: Envs) -> Self {
    resolved_environment(&envs.context)
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
  map: ResolvedEnvContext,
  keys: Vec<String>,
  position: usize,
}

impl EnvsIter {
  fn new(map: EnvContext) -> Self {
    let map = resolved_environment(&map);
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
  use std::fs;

  use serde_json::json;
  use tempfile::TempDir;

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
    let mut map = ResolvedEnvContext::new();
    map.insert("key".to_owned(), "value".to_owned());

    let envs = Envs::with_value(map);

    assert_eq!(envs.get("key").unwrap(), &"value".to_string());
  }

  #[test]
  fn test_with_value_and_parent() {
    let mut parent_map = ResolvedEnvContext::new();
    parent_map.insert("parent_key".to_owned(), "parent_value".to_owned());

    let mut map = ResolvedEnvContext::new();
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
    let mut context = ResolvedEnvContext::new();
    context.insert("key".to_string(), "value".to_string());
    envs.extend(context);

    assert_eq!(envs.get("key").unwrap(), &"value".to_string());
  }

  #[test]
  fn test_envs_iterator() {
    let mut map = ResolvedEnvContext::new();
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
    let mut map = ResolvedEnvContext::new();
    map.insert("key".to_owned(), "value".to_owned());

    let original = Envs::with_value(map);

    let serialized = serde_json::to_string(&original).unwrap();
    let deserialized: Envs = serde_json::from_str(&serialized).unwrap();

    assert_eq!(original, deserialized);
  }

  #[test]
  fn test_display() {
    let mut map = ResolvedEnvContext::new();
    map.insert("key".to_owned(), "value".to_owned());

    let envs = Envs::with_value(map);

    let display = format!("{}", envs);

    assert!(display.contains("\"key\": \"value\""));
  }

  #[test]
  fn debug_output_never_exposes_environment_values() {
    let envs = Envs::with_value(HashMap::from([("TOKEN".to_owned(), "do-not-log".to_owned())]));

    let debug = format!("{envs:?}");

    assert!(debug.contains("TOKEN"));
    assert!(!debug.contains("do-not-log"));
  }

  #[test]
  fn test_context_conversion() {
    let mut context = ResolvedEnvContext::new();
    context.insert("key".to_owned(), "value".to_owned());

    // Test From<Context> for Envs
    let envs: Envs = context.clone().into();
    assert_eq!(envs.get("key").unwrap(), &"value".to_string());

    // Test From<Envs> for Context
    let context_back: ResolvedEnvContext = envs.into();
    assert_eq!(context_back.get("key").unwrap(), &"value".to_string());
  }

  #[tokio::test]
  async fn test_expand_simple() {
    let mut context = ResolvedEnvContext::new();
    context.insert("name".to_owned(), "John".to_owned());
    context.insert("key".to_owned(), "$name".to_owned());

    let mut envs = Envs::with_value(context);

    envs.expand().await.unwrap();
    envs.expand().await.unwrap();
    assert_eq!(envs.get("name").unwrap(), &"John".to_string());
  }

  #[tokio::test]
  async fn test_expand_with_parent() {
    let mut parent_context = ResolvedEnvContext::new();
    parent_context.insert("first".to_owned(), "John".to_owned());
    let mut context = ResolvedEnvContext::new();
    context.insert("full".to_owned(), "$first Doe".to_owned());

    let parent = Envs::with_value(parent_context);

    let mut envs = Envs::with_value_and_parent(context, parent);

    envs.expand().await.unwrap();
    assert_eq!(envs.get("full").unwrap(), &"John Doe".to_string());
  }

  #[tokio::test]
  async fn test_expand_from_process_environment() {
    let mut context = ResolvedEnvContext::new();
    context.insert("path".to_owned(), "$PATH".to_owned());
    let mut envs = Envs::with_value(context);

    envs.expand().await.unwrap();

    assert_eq!(envs.get("path"), Some(&env::var("PATH").unwrap()));
  }

  #[test]
  fn expands_vars_and_shell_commands() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(temp_dir.path().join("value.txt"), "from-shell").unwrap();
    #[cfg(windows)]
    let command = "type value.txt";
    #[cfg(not(windows))]
    let command = "cat value.txt";
    #[cfg(windows)]
    let env_command = "echo %DOTENV_VALUE%";
    #[cfg(not(windows))]
    let env_command = "echo $DOTENV_VALUE";

    let mut envs = Envs::with_value(HashMap::from([
      ("DOTENV_VALUE".to_owned(), EnvValue::from("available")),
      ("FROM_VAR".to_owned(), EnvValue::from("{{ VERSION }}")),
      (
        "FROM_SHELL".to_owned(),
        EnvValue::from(format!("{{{{ shell(command=\"{command}\") }}}}")),
      ),
      (
        "FROM_DOTENV".to_owned(),
        EnvValue::Shell(octa_octafile::ShellValue {
          sh: env_command.to_owned(),
        }),
      ),
      (
        "FROM_FILTER".to_owned(),
        EnvValue::from(format!("prefix-{{{{ \"{command}\" | shell }}}}")),
      ),
    ]));
    envs.set_dir(temp_dir.path());
    let vars = Vars::with_value(json!({ "VERSION": "1.2.3" }));

    envs.expand_with(&vars, false).unwrap();

    assert_eq!(envs.get("FROM_VAR"), Some(&"1.2.3".to_owned()));
    assert_eq!(envs.get("FROM_SHELL"), Some(&"from-shell".to_owned()));
    assert_eq!(envs.get("FROM_DOTENV"), Some(&"available".to_owned()));
    assert_eq!(envs.get("FROM_FILTER"), Some(&"prefix-from-shell".to_owned()));
  }

  #[test]
  fn dry_mode_does_not_execute_env_shell_commands() {
    let temp_dir = TempDir::new().unwrap();
    let marker = temp_dir.path().join("marker.txt");
    #[cfg(windows)]
    let command = "echo executed>marker.txt";
    #[cfg(not(windows))]
    let command = "touch marker.txt";
    let mut envs = Envs::with_value(HashMap::from([(
      "VALUE".to_owned(),
      EnvValue::Shell(octa_octafile::ShellValue { sh: command.to_owned() }),
    )]));
    envs.set_dir(temp_dir.path());

    envs.expand_with(&Vars::new(), true).unwrap();

    assert!(!marker.exists());
  }

  #[test]
  fn reports_failed_env_shell_commands() {
    let temp_dir = TempDir::new().unwrap();
    #[cfg(windows)]
    let command = "echo failed 1>&2 & exit /B 7";
    #[cfg(not(windows))]
    let command = "echo failed >&2; exit 7";
    let mut envs = Envs::with_value(HashMap::from([(
      "VALUE".to_owned(),
      EnvValue::Shell(octa_octafile::ShellValue { sh: command.to_owned() }),
    )]));
    envs.set_dir(temp_dir.path());

    let error = envs.expand_with(&Vars::new(), false).unwrap_err();

    assert!(error.to_string().contains("status 7"));
    assert!(error.to_string().contains("failed"));
  }

  #[tokio::test]
  async fn shell_errors_redact_secret_variable_values() {
    let secret = "do-not-log-this-value";
    let values: octa_octafile::Vars =
      serde_yml::from_str(&format!("TOKEN:\n  value: {secret}\n  secret: true\n")).unwrap();
    let mut vars = Vars::with_variables(values);
    vars.expand(false).await.unwrap();
    #[cfg(windows)]
    let command = "echo {{ TOKEN }} 1>&2 & exit /B 7";
    #[cfg(not(windows))]
    let command = "echo {{ TOKEN }} >&2; exit 7";
    let mut envs = Envs::with_value(HashMap::from([(
      "VALUE".to_owned(),
      EnvValue::Shell(octa_octafile::ShellValue { sh: command.to_owned() }),
    )]));

    let error = envs.expand_with(&vars, false).unwrap_err().to_string();

    assert!(!error.contains(secret));
    assert!(error.contains("*****"));
  }

  #[test]
  fn test_iteration_and_multiline_display() {
    let context = HashMap::from([
      ("first".to_owned(), "one".to_owned()),
      ("second".to_owned(), "two".to_owned()),
    ]);
    let envs = Envs::with_value(context.clone());

    assert_eq!(envs.clone().into_iter().collect::<HashMap<_, _>>(), context);
    assert_eq!((&envs).into_iter().collect::<HashMap<_, _>>(), context);
    assert!(envs.to_string().contains(",\n"));
  }
}
