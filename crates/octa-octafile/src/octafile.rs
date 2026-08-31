use std::{
  collections::{HashMap, HashSet},
  env, fmt,
  fs::File,
  io::Read,
  path::{Path, PathBuf},
  str::FromStr,
  sync::{Arc, Mutex, OnceLock},
  time::Duration,
};

use indexmap::IndexMap;
use serde::{de::DeserializeSeed, Deserialize, Deserializer, Serialize};
use serde_yml::Value;
use tera::{Context as TeraContext, Tera};
use tracing::{debug, info};

use crate::{
  error::{OctafileError, OctafileResult},
  include::IncludeInfo,
  parser::{self, location_error, Node},
  task::{AllowedRun, Context, PluginCommand, PluginSchemas, Task, TaskSeed},
  variable::Variable,
};

const OCTAFILE_DEFAULT_NAMES: [&str; 8] = [
  "Octafile.yml",
  "octafile.yml",
  "Octafile.yaml",
  "octafile.yaml",
  "Octafile.lock.yml",
  "octafile.lock.yml",
  "Octafile.lock.yaml",
  "octafile.lock.yaml",
];

/// Variables retain YAML declaration order so a value can reference variables declared before it.
pub type Vars = IndexMap<String, Variable>;
pub type Envs = HashMap<String, EnvValue>;

/// A literal environment value or a command that produces it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum EnvValue {
  String(String),
  Shell(ShellValue),
}

impl EnvValue {
  pub fn as_str(&self) -> Option<&str> {
    match self {
      Self::String(value) => Some(value),
      Self::Shell(_) => None,
    }
  }
}

impl From<String> for EnvValue {
  fn from(value: String) -> Self {
    Self::String(value)
  }
}

impl From<&str> for EnvValue {
  fn from(value: &str) -> Self {
    Self::String(value.to_owned())
  }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ShellValue {
  pub sh: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WatchInterval(Duration);

impl WatchInterval {
  pub fn duration(self) -> Duration {
    self.0
  }
}

impl FromStr for WatchInterval {
  type Err = String;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    let value = value.trim();
    let (number, multiplier) = if let Some(number) = value.strip_suffix("ms") {
      (number, 1)
    } else if let Some(number) = value.strip_suffix('s') {
      (number, 1_000)
    } else if let Some(number) = value.strip_suffix('m') {
      (number, 60_000)
    } else {
      return Err("expected a duration ending in ms, s, or m".to_string());
    };

    let number = number
      .parse::<u64>()
      .map_err(|_| "expected a positive integer duration".to_string())?;
    let millis = number
      .checked_mul(multiplier)
      .ok_or_else(|| "duration is too large".to_string())?;
    if millis == 0 {
      return Err("duration must be greater than zero".to_string());
    }

    Ok(Self(Duration::from_millis(millis)))
  }
}

impl<'de> Deserialize<'de> for WatchInterval {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    String::deserialize(deserializer)?
      .parse()
      .map_err(serde::de::Error::custom)
  }
}

/// Enum of available file versions
#[derive(Deserialize, Serialize, Debug, Clone, Copy, Default)]
#[serde(try_from = "u8")]
pub enum Version {
  #[default]
  V1 = 1,
}

impl TryFrom<u8> for Version {
  type Error = String;

  fn try_from(value: u8) -> Result<Self, Self::Error> {
    match value {
      1 => Ok(Version::V1),
      _ => Err(format!("Unsupported version: {}", value)),
    }
  }
}

impl From<Version> for u8 {
  fn from(version: Version) -> Self {
    version as u8
  }
}

impl PartialEq<u8> for Version {
  fn eq(&self, other: &u8) -> bool {
    (*self as u8) == *other
  }
}

impl fmt::Display for Version {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
      Version::V1 => write!(f, "1"),
    }
  }
}

/// Main taskfile structure representing the entire configuration
#[derive(Default)]
pub struct Octafile {
  // Octafile schema version
  pub version: Version,

  // Octafile global vars
  pub vars: Option<Vars>,

  // Octafile global environment variables
  pub env: Option<Envs>,

  // Environment files applied to tasks in this Octafile
  pub dotenv: Option<Vec<String>>,

  // Default task run mode
  pub run: Option<AllowedRun>,

  // Stop parallel execution after the first failure
  pub failfast: Option<bool>,

  // Watch polling interval
  pub interval: Option<WatchInterval>,

  // Plugin used by short task and command forms
  pub default_plugin: String,

  // list of included octafiles
  pub includes: Option<HashMap<String, IncludeInfo>>,

  // List of task
  pub tasks: HashMap<String, Task>,

  // Working directory for the octafile
  // #[serde(skip)]
  pub dir: PathBuf,

  // Name of octafile
  // #[serde(skip)]
  _name: String,

  // Internal list of octafiles
  // #[serde(skip)]
  _included: Mutex<HashMap<String, Arc<Octafile>>>,

  // Parent octafile
  // #[serde(skip)]
  _parent: Option<Arc<Octafile>>,

  // Self reference to octafile
  // #[serde(skip)]
  _self: OnceLock<Arc<Octafile>>,
}

/// Custom Debug implementation to avoid cyclic reference
/// on self field
impl fmt::Debug for Octafile {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Octafile")
      .field("version", &self.version)
      .field("name", &self._name)
      .field("dotenv", &self.dotenv)
      .field("run", &self.run)
      .field("failfast", &self.failfast)
      .field("interval", &self.interval)
      .field("default_plugin", &self.default_plugin)
      .field("includes", &self.includes)
      .field("tasks", &self.tasks)
      .field("dir", &self.dir)
      .finish()
  }
}

impl Octafile {
  pub fn load(
    path: Option<PathBuf>,
    global: bool,
    plugin_keys: Vec<String>,
    default_plugin: impl Into<String>,
  ) -> OctafileResult<Arc<Self>> {
    let context = Context::from_keys(plugin_keys, default_plugin).map_err(OctafileError::PluginSchemaError)?;
    Self::load_with_context(path, global, None, context)
  }

  pub fn load_with_schemas(
    path: Option<PathBuf>,
    global: bool,
    schemas: PluginSchemas,
    default_plugin: impl Into<String>,
  ) -> OctafileResult<Arc<Self>> {
    Self::load_with_schemas_from(path, global, None, schemas, default_plugin)
  }

  /// Loads an Octafile, starting automatic discovery from `search_dir` when provided.
  pub fn load_with_schemas_from(
    path: Option<PathBuf>,
    global: bool,
    search_dir: Option<PathBuf>,
    schemas: PluginSchemas,
    default_plugin: impl Into<String>,
  ) -> OctafileResult<Arc<Self>> {
    let context = Context::from_schemas(schemas, default_plugin).map_err(OctafileError::PluginSchemaError)?;
    Self::load_with_context(path, global, search_dir, context)
  }

  fn load_with_context(
    path: Option<PathBuf>,
    global: bool,
    search_dir: Option<PathBuf>,
    context: Context,
  ) -> OctafileResult<Arc<Self>> {
    let search_dir = match search_dir {
      Some(path) if path.is_absolute() => Some(path),
      Some(path) => Some(env::current_dir()?.join(path)),
      None => None,
    };

    let path = match path {
      Some(path) => {
        let path = match &search_dir {
          Some(search_dir) if path.is_relative() => search_dir.join(path),
          _ => path,
        };
        Octafile::find_octafile(Some(path))
      },
      None => {
        if global {
          let home = home_dir().ok_or(OctafileError::NotSearchedError)?;
          Octafile::find_octafile(Some(home))
        } else {
          match search_dir {
            Some(search_dir) => Octafile::find_octafile_from(search_dir),
            None => Octafile::find_octafile(None),
          }
        }
      },
    }?
    .ok_or(OctafileError::NotSearchedError)?;
    let path = path.canonicalize().map_err(OctafileError::IoError)?;

    debug!("Loading octafile: {}", path.display());

    let mut octafile = Self::read_octafile(&path, &context)?;
    octafile.set_attributes(&path)?;
    octafile._self = OnceLock::new();

    let octafile = Arc::new(octafile);
    let _ = octafile._self.set(Arc::clone(&octafile));
    Self::load_includes(Arc::clone(&octafile), &context)?;

    Ok(octafile)
  }

  fn deserialize_with_context(value: Node, context: &Context) -> Result<Self, String> {
    let map = value.into_mapping().ok_or_else(|| "Expected mapping".to_string())?;
    let mut fields = HashSet::new();

    for (key, _) in &map {
      let key_marker = key.marker();
      let key = key
        .as_str()
        .ok_or_else(|| location_error(key_marker, "Octafile keys must be strings"))?;

      if !fields.insert(key.to_owned()) {
        return Err(location_error(key_marker, &format!("duplicated key '{key}'")));
      }
    }

    // Resolve the file-level override before tasks so YAML field order cannot affect short forms.
    let default_plugin = map
      .iter()
      .find(|(key, _)| key.as_str() == Some("default_plugin"))
      .map(|(_, value)| serde_yml::from_value::<String>(value.clone().into_value()?).map_err(|error| error.to_string()))
      .transpose()?;
    let context = context.with_default_plugin(default_plugin)?;
    let mut octafile = Octafile {
      default_plugin: context.default_plugin().to_owned(),
      ..Octafile::default()
    };

    for (key, value) in map {
      let key_marker = key.marker();
      let key = key
        .as_str()
        .ok_or_else(|| location_error(key_marker, "Octafile keys must be strings"))?;

      match key {
        "version" => {
          octafile.version = match value.into_value()? {
            Value::Number(n) => Version::try_from(n.as_u64().unwrap_or(0) as u8).map_err(|e| e.to_string())?,
            _ => return Err("Version must be a number".to_string()),
          };
        },
        "vars" => {
          octafile.vars = serde_yml::from_value(value.into_value()?).map_err(|e| e.to_string())?;
        },
        "env" => {
          octafile.env = serde_yml::from_value(value.into_value()?).map_err(|e| e.to_string())?;
        },
        "dotenv" => {
          octafile.dotenv = serde_yml::from_value(value.into_value()?).map_err(|e| e.to_string())?;
        },
        "run" => {
          octafile.run = serde_yml::from_value(value.into_value()?).map_err(|e| e.to_string())?;
        },
        "failfast" => {
          octafile.failfast = serde_yml::from_value(value.into_value()?).map_err(|e| e.to_string())?;
        },
        "interval" => {
          octafile.interval = serde_yml::from_value(value.into_value()?).map_err(|e| e.to_string())?;
        },
        "default_plugin" => {},
        "includes" => {
          octafile.includes = serde_yml::from_value(value.into_value()?).map_err(|e| e.to_string())?;
        },
        "tasks" => {
          let tasks_map = value
            .into_mapping()
            .ok_or_else(|| "Expected mapping for tasks".to_string())?;
          let mut tasks = HashMap::new();

          for (task_key, task_value) in tasks_map {
            let task_marker = task_key.marker();
            let task_name = task_key
              .as_str()
              .ok_or_else(|| location_error(task_marker, "task names must be strings"))?
              .to_owned();
            let annotation = task_value.annotation().map(str::to_owned);

            let task = if let Some(annotation) = annotation {
              if annotation.is_empty() || !context.contains(&annotation) {
                return Err(location_error(
                  task_value.marker(),
                  &format!("unknown task annotation '!{annotation}'"),
                ));
              }

              let marker = task_value.marker();
              let value = task_value.into_untagged_value()?;
              context
                .validate(&annotation, &value)
                .map_err(|error| location_error(marker, &error))?;

              Task {
                plugin: Some(PluginCommand { key: annotation, value }),
                ..Task::default()
              }
            } else {
              let value = task_value.into_value()?;
              let task_seed = TaskSeed { context: &context };
              let deserializer = serde_yml::Deserializer::new(&value);
              task_seed.deserialize(deserializer).map_err(|e| e.to_string())?
            };

            if tasks.insert(task_name.clone(), task).is_some() {
              return Err(location_error(task_marker, &format!("duplicated task '{task_name}'")));
            }
          }

          octafile.tasks = tasks;
        },
        _ => return Err(location_error(key_marker, &format!("unknown Octafile field '{key}'"))),
      }
    }

    Ok(octafile)
  }

  /// Return specified included octafile
  pub fn get_included(&self, name: &str) -> OctafileResult<Option<Arc<Octafile>>> {
    self
      ._included
      .lock()
      .map_err(|_| OctafileError::LockError("Failed to lock included octafiles".to_string()))
      .map(|guard| guard.get(name).map(Arc::clone))
  }

  /// Return map of all included octafiles
  pub fn get_all_included(&self) -> OctafileResult<HashMap<String, Arc<Octafile>>> {
    self
      ._included
      .lock()
      .map_err(|e| OctafileError::LockError(format!("Failed to lock included octafiles: {}", e)))
      .map(|guard| {
        guard
          .iter()
          .map(|(k, v)| (k.clone(), Arc::clone(v)))
          .collect::<HashMap<_, _>>()
      })
  }

  /// Return reference to parent octafile
  pub fn parent(&self) -> Option<&Arc<Octafile>> {
    self._parent.as_ref()
  }

  /// Return reference to root octafile
  pub fn root(&self) -> &Arc<Octafile> {
    match &self._parent {
      Some(parent) => parent.root(),
      None => self._self.get().unwrap(),
    }
  }

  /// Return true if current octafile is root
  pub fn is_root(&self) -> bool {
    self._parent.is_none()
  }

  pub fn name(&self) -> String {
    self._name.clone()
  }

  /// Return path from root octafile to current
  pub fn hierarchy_path(&self) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = self;

    while let Some(parent) = current.parent() {
      path.push(current._name.clone());
      current = parent;
    }

    path.reverse();
    path
  }

  /// Load including octafiles
  fn load_includes(octafile: Arc<Octafile>, context: &Context) -> OctafileResult<()> {
    let includes = match &octafile.includes {
      Some(includes) => includes,
      None => return Ok(()),
    };

    // Each include inherits the effective key of its parent; its own file may override it.
    let context = context
      .with_default_plugin(Some(octafile.default_plugin.clone()))
      .map_err(OctafileError::PluginSchemaError)?;

    for (name, include) in includes {
      let (path_template, optional) = match include {
        IncludeInfo::Simple(path) => (path.as_str(), false),
        IncludeInfo::Complex(complex) => (complex.octafile.as_str(), complex.optional.unwrap_or(false)),
      };
      let rendered_path = Self::render_include_path(&octafile, path_template)?;
      let unresolved_path = octafile.dir.join(&rendered_path);
      let path = match unresolved_path.canonicalize() {
        Ok(path) => Octafile::find_octafile(Some(path))?
          .ok_or_else(|| OctafileError::NotFoundError(unresolved_path.display().to_string()))?,
        Err(_) if optional => {
          info!(
            "Skipping optional {} octafile. Reason:: not found",
            unresolved_path.display()
          );
          continue;
        },
        Err(_) => return Err(OctafileError::NotFoundError(unresolved_path.display().to_string())),
      };

      debug!("Loading included octafile: {}", path.display());
      let mut include_octafile = match Self::read_octafile(&path, &context) {
        Ok(mut t) => {
          t._parent = Some(Arc::clone(&octafile));
          t._name = name.clone();

          if let IncludeInfo::Complex(inc_info) = include {
            if let Some(vars) = inc_info.vars.clone() {
              t.vars = match t.vars.take() {
                Some(mut file_vars) => {
                  file_vars.extend(vars);
                  Some(file_vars)
                },
                None => Some(vars),
              };
            }
          }

          t
        },
        Err(OctafileError::NotFoundError(e)) => {
          if let IncludeInfo::Complex(inc_info) = include {
            if inc_info.optional.unwrap_or(false) {
              info!("Skipping optional {} octafile. Reason:: not found", path.display());

              continue;
            }
          }

          return Err(OctafileError::NotFoundError(e));
        },
        Err(e) => return Err(e),
      };

      include_octafile.set_attributes(&path)?;
      let include_octafile = Arc::new(include_octafile);

      // Recursively process nested includes
      if include_octafile.includes.is_some() {
        Self::load_includes(Arc::clone(&include_octafile), &context)?;
      }

      octafile
        ._included
        .lock()
        .map_err(|_| OctafileError::LockError("Failed to lock included octafiles".to_string()))?
        .insert(name.clone(), include_octafile);
    }

    Ok(())
  }

  /// Resolves an include template before filesystem lookup using Go-compatible platform names.
  fn render_include_path(octafile: &Octafile, path: &str) -> OctafileResult<String> {
    let mut template_context = match &octafile.vars {
      Some(vars) => {
        // Secret values must not become filesystem paths, which are exposed by diagnostics and IO errors.
        let values: IndexMap<_, _> = vars
          .iter()
          .filter(|(_, variable)| !variable.is_secret())
          .map(|(key, variable)| (key, variable.template_value()))
          .collect();
        TeraContext::from_serialize(values)
      },
      None => Ok(TeraContext::new()),
    }
    .map_err(|error| OctafileError::IncludeTemplateError(path.to_owned(), error.to_string()))?;
    template_context.insert("OS", go_os());
    template_context.insert("ARCH", go_arch());

    Tera::one_off(path, &template_context, false)
      .map_err(|error| OctafileError::IncludeTemplateError(path.to_owned(), error.to_string()))
  }

  /// Sets common attributes for an Octafile, including merging from parent if present
  fn set_attributes(&mut self, path: &Path) -> OctafileResult<()> {
    // Set working directory
    let octafile_dir = path.parent().ok_or_else(|| {
      OctafileError::IoError(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "Parent directory not found",
      ))
    })?;
    self.dir = octafile_dir.to_path_buf();

    Ok(())
  }

  /// Reads and parses a taskfile from the given path
  fn read_octafile<P: AsRef<Path>>(taskfile_path: P, context: &Context) -> OctafileResult<Octafile> {
    let path = taskfile_path.as_ref();
    let path_str = path.display().to_string();

    let mut file = File::open(path).map_err(|e| match e.kind() {
      std::io::ErrorKind::NotFound => OctafileError::NotFoundError(path_str.clone()),
      _ => OctafileError::IoError(e),
    })?;

    let mut content = String::new();
    file
      .read_to_string(&mut content)
      .map_err(|_| OctafileError::ReadError(path_str.clone()))?;

    let yaml_value = parser::parse(&content).map_err(|error| OctafileError::ParseError(path_str.clone(), error))?;

    Octafile::deserialize_with_context(yaml_value, context).map_err(|e| OctafileError::ParseError(path_str, e))
  }

  /// Try to find octafile config traversing to root directory from current directory
  fn find_octafile(path: Option<PathBuf>) -> OctafileResult<Option<PathBuf>> {
    if let Some(path) = path {
      if path.is_dir() {
        for taskfile_name in OCTAFILE_DEFAULT_NAMES {
          let potential_path = path.join(taskfile_name);
          if potential_path.is_file() {
            return Ok(Some(potential_path));
          }
        }
      } else {
        return Ok(Some(path));
      }
    } else {
      return Self::find_octafile_from(env::current_dir()?);
    }

    Ok(None)
  }

  fn find_octafile_from(mut current_dir: PathBuf) -> OctafileResult<Option<PathBuf>> {
    if !current_dir.is_dir() {
      return Ok(None);
    }

    loop {
      for taskfile_name in OCTAFILE_DEFAULT_NAMES {
        let potential_path = current_dir.join(taskfile_name);
        if potential_path.is_file() {
          return Ok(Some(potential_path));
        }
      }

      if !current_dir.pop() {
        return Ok(None);
      }
    }
  }
}

fn home_dir() -> Option<PathBuf> {
  #[cfg(windows)]
  let home = env::var_os("USERPROFILE");
  #[cfg(not(windows))]
  let home = env::var_os("HOME");

  home
    .filter(|path| !path.is_empty())
    .map(PathBuf::from)
    .or_else(dirs::home_dir)
}

fn go_os() -> &'static str {
  go_os_name(env::consts::OS)
}

fn go_os_name(os: &str) -> &str {
  match os {
    "macos" => "darwin",
    os => os,
  }
}

fn go_arch() -> &'static str {
  go_arch_name(env::consts::ARCH, cfg!(target_endian = "little"))
}

fn go_arch_name(arch: &str, little_endian: bool) -> &str {
  match arch {
    "x86" => "386",
    "x86_64" => "amd64",
    "aarch64" => "arm64",
    "loongarch64" => "loong64",
    "mips" if little_endian => "mipsle",
    "mips64" if little_endian => "mips64le",
    "powerpc64" if little_endian => "ppc64le",
    "powerpc64" => "ppc64",
    "wasm32" => "wasm",
    arch => arch,
  }
}

#[cfg(test)]
mod tests {
  use crate::task::{CommandPayload, Context};

  use super::{go_arch, go_arch_name, go_os, go_os_name, OCTAFILE_DEFAULT_NAMES};
  use crate::*;
  use octafile::Version;
  use pretty_assertions::assert_eq;
  use serde_yml::Value;
  use std::env;
  use std::fs;
  use std::path::PathBuf;
  use std::sync::Arc;
  use std::time::Duration;
  use tempfile::{Builder, TempDir};

  const TEST_DEFAULT_PLUGIN: &str = "shell";

  fn create_temp_octafile(content: &str, prefix: &str) -> (TempDir, PathBuf) {
    let temp_dir = Builder::new().prefix(prefix).tempdir().unwrap();
    let file_path = temp_dir.path().join("Octafile.yml");
    fs::write(&file_path, content).unwrap();
    (temp_dir, file_path)
  }

  fn task_mapping(value: &Value) -> &serde_yml::Mapping {
    match value {
      Value::Mapping(mapping) => mapping,
      _ => panic!("expected a mapping"),
    }
  }

  fn shell_condition(value: &str) -> PluginCommand {
    PluginCommand {
      key: TEST_DEFAULT_PLUGIN.to_string(),
      value: Value::String(value.to_string()),
    }
  }

  #[test]
  fn file_default_plugin_applies_to_all_short_forms_regardless_of_field_order() {
    let content = r#"
      version: 1
      tasks:
        direct: render direct
        pipeline:
          if: render condition
          cmds:
            - render command
      default_plugin: tpl
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "file_default_plugin");

    let octafile = Octafile::load(
      Some(file_path),
      false,
      vec!["shell".to_string(), "tpl".to_string()],
      "shell",
    )
    .unwrap();

    assert_eq!(octafile.default_plugin, "tpl");
    let direct = octafile.tasks["direct"].plugin.as_ref().unwrap();
    assert_eq!(direct.key, "tpl");
    assert_eq!(direct.value, Value::String("render direct".to_string()));
    let condition = octafile.tasks["pipeline"]
      .condition
      .as_ref()
      .and_then(|conditions| conditions.after_deps.as_ref())
      .unwrap();
    assert_eq!(condition.command.key, "tpl");
    assert!(matches!(
      &octafile.tasks["pipeline"].cmds.as_ref().unwrap()[0].payload,
      CommandPayload::Plugin(plugin) if plugin.key == "tpl"
    ));
  }

  #[test]
  fn included_octafiles_inherit_and_override_default_plugin() {
    let temp_dir = TempDir::new().unwrap();
    let inherited_path = temp_dir.path().join("inherited.yml");
    let overridden_path = temp_dir.path().join("overridden.yml");
    let root_path = temp_dir.path().join("Octafile.yml");
    fs::write(&inherited_path, "version: 1\ntasks:\n  child: inherited command\n").unwrap();
    fs::write(
      &overridden_path,
      "version: 1\ndefault_plugin: shell\ntasks:\n  child: overridden command\n",
    )
    .unwrap();
    fs::write(
      &root_path,
      format!(
        "version: 1\ndefault_plugin: tpl\nincludes:\n  inherited: {}\n  overridden: {}\ntasks: {{}}\n",
        inherited_path.display(),
        overridden_path.display()
      ),
    )
    .unwrap();

    let root = Octafile::load(
      Some(root_path),
      false,
      vec!["shell".to_string(), "tpl".to_string()],
      "shell",
    )
    .unwrap();
    let inherited = root.get_included("inherited").unwrap().unwrap();
    let overridden = root.get_included("overridden").unwrap().unwrap();

    assert_eq!(inherited.default_plugin, "tpl");
    assert_eq!(inherited.tasks["child"].plugin.as_ref().unwrap().key, "tpl");
    assert_eq!(overridden.default_plugin, "shell");
    assert_eq!(overridden.tasks["child"].plugin.as_ref().unwrap().key, "shell");
  }

  #[test]
  fn rejects_unknown_file_default_plugin() {
    let content = "version: 1\ndefault_plugin: missing\ntasks: {}\n";
    let (_temp_dir, file_path) = create_temp_octafile(content, "unknown_default_plugin");

    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();

    assert!(error.to_string().contains("unknown default plugin 'missing'"));
  }

  #[test]
  fn rejects_unregistered_initial_default_plugin() {
    let error = Context::from_keys(vec!["tpl".to_string()], "shell").err().unwrap();

    assert_eq!(error, "unknown default plugin 'shell'");
  }

  #[test]
  fn test_load_basic_octafile() {
    let content = r#"
      version: 1
      run: changed
      interval: 250ms
      tasks:
        test:
          watch: true
          shell: echo "hello"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "load_basic_octafile");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();
    assert_eq!(octafile.version, 1);
    assert_eq!(octafile.run, Some(AllowedRun::Changed));
    assert_eq!(octafile.interval.unwrap().duration(), Duration::from_millis(250));
    assert_eq!(octafile.tasks["test"].watch, Some(true));
    assert!(octafile.tasks.contains_key("test"));
  }

  #[test]
  fn test_invalid_octafile_run_mode() {
    let content = r#"
      version: 1
      run: invalid
      tasks: {}
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_octafile_run_mode");

    assert!(Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").is_err());
  }

  #[test]
  fn test_invalid_watch_interval() {
    for interval in ["0ms", "100", "soon"] {
      let content = format!(
        r#"
        version: 1
        interval: {interval}
        tasks: {{}}
      "#
      );
      let (_temp_dir, file_path) = create_temp_octafile(&content, "invalid_watch_interval");

      assert!(Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").is_err());
    }
  }

  #[test]
  fn test_mixed_task_values() {
    let content = r#"
      version: 1
      tasks:
        simple_string: echo "simple"
        complex_map:
          desc: "A complex task"
          cmds:
            - echo "complex"
        another_string: echo "another"
      "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "mixed_task_values");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();
    assert_eq!(octafile.version, 1);

    // Test string task
    let simple_task = &octafile.tasks["simple_string"];
    let plugin = simple_task.plugin.as_ref().unwrap();
    assert_eq!(plugin.key, "shell");
    assert_eq!(plugin.value, Value::String("echo \"simple\"".to_string()));

    // Test complex task
    let complex_task = &octafile.tasks["complex_map"];
    assert_eq!(complex_task.desc, Some("A complex task".to_string()));
    assert!(complex_task.cmds.is_some());

    // Test another string task
    let another_task = &octafile.tasks["another_string"];
    let plugin = another_task.plugin.as_ref().unwrap();
    assert_eq!(plugin.key, "shell");
    assert_eq!(plugin.value, Value::String("echo \"another\"".to_string()));
  }

  #[test]
  fn test_nested_includes() {
    let root_content = r#"
      version: 1
      includes:
        child:
          octafile: child/Octafile.yml
      tasks:
        root_task:
          shell: echo "root"
    "#;

    let child_content = r#"
      version: 1
      tasks:
        child_task:
          shell: echo "child"
    "#;

    let temp_dir = Builder::new().prefix("nested_includes").tempdir().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");
    let child_dir = temp_dir.path().join("child");
    fs::create_dir(&child_dir).unwrap();
    let child_path = child_dir.join("Octafile.yml");

    fs::write(&root_path, root_content).unwrap();
    fs::write(&child_path, child_content).unwrap();

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();

    // Test basic structure
    assert_eq!(root._name, "".to_string());
    assert!(root.is_root());

    // Test includes
    let child = root.get_included("child").unwrap().unwrap();
    assert_eq!(child._name, "child".to_string());
    assert!(!child.is_root());

    // Test hierarchy
    assert_eq!(child.hierarchy_path(), vec!["child".to_string()]);
  }

  #[test]
  fn interpolates_platform_and_octafile_vars_in_include_paths() {
    let temp_dir = TempDir::new().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");
    let included_path = temp_dir
      .path()
      .join(format!("Taskfile_{}_{}_debug.yml", go_os(), go_arch()));
    fs::write(
      &root_path,
      r#"
version: 1
vars:
  PROFILE: debug
includes:
  platform: Taskfile_{{OS}}_{{ARCH}}_{{PROFILE}}.yml
tasks: {}
"#,
    )
    .unwrap();
    fs::write(
      included_path,
      r#"
version: 1
tasks:
  build: echo platform
"#,
    )
    .unwrap();

    let octafile = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();
    let included = octafile.get_included("platform").unwrap().unwrap();

    assert!(included.tasks.contains_key("build"));
  }

  #[test]
  fn uses_go_platform_names_in_include_templates() {
    assert_eq!(go_os_name("macos"), "darwin");
    assert_eq!(go_os_name("linux"), "linux");

    for (architecture, expected) in [
      ("x86", "386"),
      ("x86_64", "amd64"),
      ("aarch64", "arm64"),
      ("loongarch64", "loong64"),
      ("wasm32", "wasm"),
    ] {
      assert_eq!(go_arch_name(architecture, false), expected);
    }
    assert_eq!(go_arch_name("mips", true), "mipsle");
    assert_eq!(go_arch_name("mips", false), "mips");
    assert_eq!(go_arch_name("mips64", true), "mips64le");
    assert_eq!(go_arch_name("mips64", false), "mips64");
    assert_eq!(go_arch_name("powerpc64", true), "ppc64le");
    assert_eq!(go_arch_name("powerpc64", false), "ppc64");
    assert_eq!(go_arch_name("riscv64", true), "riscv64");
  }

  #[test]
  fn reports_invalid_include_path_templates() {
    let content = r#"
      version: 1
      includes:
        platform: Taskfile_{{MISSING}}.yml
      tasks: {}
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_include_template");

    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();

    assert!(matches!(error, OctafileError::IncludeTemplateError(_, _)));
  }

  #[test]
  fn does_not_expose_secret_variables_to_include_templates() {
    let content = r#"
      version: 1
      vars:
        TOKEN:
          value: private-path
          secret: true
      includes:
        private: Taskfile_{{TOKEN}}.yml
      tasks: {}
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "secret_include_template");

    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();
    let message = error.to_string();

    assert!(matches!(error, OctafileError::IncludeTemplateError(_, _)));
    assert!(!message.contains("private-path"));
  }

  #[test]
  fn test_optional_includes() {
    let content = r#"
      version: 1

      includes:
        optional:
          octafile: nonexistent.yml
          optional: true
      tasks:
        root_task:
          shell: echo "root"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "optional_includes");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();
    assert!(octafile.get_included("optional").unwrap().is_none());
  }

  #[test]
  fn test_error_handling() {
    // Test nonexistent file
    assert!(matches!(
      Octafile::load(
        Some(PathBuf::from("nonexistent.yml")),
        false,
        vec!["shell".to_string()],
        "shell"
      ),
      Err(OctafileError::IoError(_))
    ));

    // Test invalid YAML
    let content = "invalid: : yaml:";
    let (_temp_dir, file_path) = create_temp_octafile(content, "error_handling");
    assert!(matches!(
      Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell"),
      Err(OctafileError::ParseError(_, _))
    ));

    // Test non-optional missing include
    let content = r#"
      version: 1
      includes:
        required:
          octafile: missing.yml
          optional: false
      tasks:
        simple:
          shell: echo "simple"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "error_handling");
    assert!(matches!(
      Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell"),
      Err(OctafileError::NotFoundError(_))
    ));
  }

  #[test]
  fn test_working_directory() {
    let content = r#"
      version: 1
      tasks:
        test:
          dir: custom_dir
          shell: echo "test"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "working_directory");

    let octafile = Octafile::load(Some(file_path.clone()), false, vec!["shell".to_string()], "shell").unwrap();
    assert_eq!(
      octafile.dir,
      file_path.canonicalize().unwrap().parent().unwrap().to_path_buf()
    );
  }

  #[test]
  fn test_root_reference_consistency() {
    let root_content = r#"
      version: 1
      includes:
        child:
          octafile: child/Octafile.yml
      tasks:
        simple:
          shell: echo "simple"
    "#;

    let child_content = r#"
      version: 1
      includes:
        grandchild:
          octafile: grandchild/Octafile.yml
      tasks:
        simple:
          shell: echo "simple"
    "#;

    let grandchild_content = r#"
      version: 1
      tasks:
        simple:
          shell: echo "simple"
    "#;

    let temp_dir = Builder::new().prefix("root_reference_consistency").tempdir().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");

    // Setup directory structure
    let child_dir = temp_dir.path().join("child");
    fs::create_dir(&child_dir).unwrap();
    let child_path = child_dir.join("Octafile.yml");

    let grandchild_dir = child_dir.join("grandchild");
    fs::create_dir(&grandchild_dir).unwrap();
    let grandchild_path = grandchild_dir.join("Octafile.yml");

    fs::write(&root_path, root_content).unwrap();
    fs::write(&child_path, child_content).unwrap();
    fs::write(&grandchild_path, grandchild_content).unwrap();

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();
    let child = root.get_included("child").unwrap().unwrap();
    let grandchild = child.get_included("grandchild").unwrap().unwrap();

    // Verify that all nodes point to the same root
    assert!(Arc::ptr_eq(&root, root.root()));
    assert!(Arc::ptr_eq(&root, child.root()));
    assert!(Arc::ptr_eq(&root, grandchild.root()));
  }

  #[test]
  fn test_find_octafile() {
    let content = r#"
      version: 1
      tasks:
        simple:
          shell: echo "simple"
    "#;

    // Test with existing Octafile
    let temp_dir = Builder::new().prefix("find_octafile").tempdir().unwrap();
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content).unwrap();

    // Create nested directory structure
    let nested_dir = temp_dir.path().join("nested").join("deeply");
    fs::create_dir_all(&nested_dir).unwrap();

    let found = Octafile::find_octafile_from(nested_dir.clone()).unwrap();
    assert_eq!(
      found.unwrap().canonicalize().unwrap(),
      octafile_path.canonicalize().unwrap()
    );
    assert!(Octafile::find_octafile_from(octafile_path.clone()).unwrap().is_none());

    // Change to nested directory and try to find Octafile
    let original_dir = env::current_dir().unwrap();
    env::set_current_dir(&nested_dir).unwrap();

    let found = Octafile::find_octafile(None).unwrap();
    assert!(found.is_some());
    assert_eq!(
      found.unwrap().canonicalize().unwrap(),
      octafile_path.canonicalize().unwrap()
    );

    // Test with different Octafile names
    env::set_current_dir(&original_dir).unwrap();
    for name in OCTAFILE_DEFAULT_NAMES {
      let temp_dir = TempDir::new().unwrap();
      let octafile_path = temp_dir.path().join(name);
      fs::write(&octafile_path, content).unwrap();

      env::set_current_dir(temp_dir.path()).unwrap();
      let found = Octafile::find_octafile(None).unwrap();
      assert!(found.is_some());
      assert_eq!(
        found.unwrap().canonicalize().unwrap(),
        octafile_path.canonicalize().unwrap()
      );
    }

    // Test with no Octafile
    let empty_dir = TempDir::new().unwrap();
    env::set_current_dir(empty_dir.path()).unwrap();
    assert!(Octafile::find_octafile(None).unwrap().is_none());

    // Restore original directory
    env::set_current_dir(original_dir).unwrap();
  }

  #[test]
  fn test_hierarchy_and_relationships() {
    let root_content = r#"
      version: 1
      includes:
        level1:
          octafile: level1/Octafile.yml
      tasks:
        simple:
          shell: echo "simple"
    "#;

    let level1_content = r#"
      version: 1
      includes:
        level2:
          octafile: level2/Octafile.yml
      tasks:
        simple:
          shell: echo "simple"
    "#;

    let level2_content = r#"
      version: 1
      tasks:
        simple:
          shell: echo "simple"
    "#;

    // Create directory structure
    let temp_dir = Builder::new().prefix("hierarchy_and_relationships").tempdir().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");

    let level1_dir = temp_dir.path().join("level1");
    fs::create_dir(&level1_dir).unwrap();
    let level1_path = level1_dir.join("Octafile.yml");

    let level2_dir = level1_dir.join("level2");
    fs::create_dir(&level2_dir).unwrap();
    let level2_path = level2_dir.join("Octafile.yml");

    // Write files
    fs::write(&root_path, root_content).unwrap();
    fs::write(&level1_path, level1_content).unwrap();
    fs::write(&level2_path, level2_content).unwrap();

    // Load root octafile
    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();

    // Test root properties
    assert!(root.is_root());
    assert!(root.parent().is_none());
    assert_eq!(root.hierarchy_path(), Vec::<String>::new());

    // Test level1
    let level1 = root.get_included("level1").unwrap().unwrap();
    assert!(!level1.is_root());
    assert!(level1.parent().is_some());
    assert_eq!(level1.hierarchy_path(), vec!["level1"]);
    assert!(Arc::ptr_eq(level1.root(), &root));

    // Test level2
    let level2 = level1.get_included("level2").unwrap().unwrap();
    assert!(!level2.is_root());
    assert!(level2.parent().is_some());
    assert_eq!(level2.hierarchy_path(), vec!["level1", "level2"]);
    assert!(Arc::ptr_eq(level2.root(), &root));

    // Test relationships
    assert!(Arc::ptr_eq(level1.parent().unwrap(), &root));
    assert!(Arc::ptr_eq(level2.parent().unwrap(), &level1));
  }

  #[test]
  fn test_get_included_methods() {
    let content = r#"
      version: 1
      includes:
        first:
          octafile: first/Octafile.yml
        second:
          octafile: second/Octafile.yml
      tasks:
        simple:
          shell: echo "simple"
    "#;

    let child_content = r#"
      version: 1
      tasks:
        simple:
          shell: echo "simple"
    "#;

    // Setup directory structure
    let temp_dir = Builder::new().prefix("get_included_methods").tempdir().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");

    let first_dir = temp_dir.path().join("first");
    fs::create_dir(&first_dir).unwrap();
    let first_path = first_dir.join("Octafile.yml");

    let second_dir = temp_dir.path().join("second");
    fs::create_dir(&second_dir).unwrap();
    let second_path = second_dir.join("Octafile.yml");

    fs::write(&root_path, content).unwrap();
    fs::write(&first_path, child_content).unwrap();
    fs::write(&second_path, child_content).unwrap();

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();

    // Test get_included
    let _ = root.get_included("first").unwrap().unwrap();
    let _ = root.get_included("second").unwrap().unwrap();
    assert!(root.get_included("nonexistent").unwrap().is_none());

    // Test get_all_included
    let all_included = root.get_all_included().unwrap();
    assert_eq!(all_included.len(), 2);
    assert!(all_included.contains_key("first"));
    assert!(all_included.contains_key("second"));
  }

  #[test]
  fn test_task_string_value() {
    let content = r#"
      version: 1
      tasks:
        simple: echo "test"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "task_string_value");
    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    let task = &octafile.tasks["simple"];
    let plugin = task.plugin.as_ref().unwrap();
    assert_eq!(plugin.key, "shell");
    assert_eq!(plugin.value, Value::String("echo \"test\"".to_string()));
  }

  #[test]
  fn test_task_complex_value() {
    let content = r#"
      version: 1
      tasks:
        complex:
          desc: "Complex task"
          cmds:
            - echo "step 1"
            - echo "step 2"
          env:
            TEST_VAR: "test value"
          dir: "./test"
          platforms:
            - linux
            - macos
          ignore_error: true
          deps:
            - task: other_task
              vars:
                key: value
          silent: true
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "task_complex_value");
    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    let task = &octafile.tasks["complex"];
    assert_eq!(task.desc, Some("Complex task".to_string()));
    assert!(task.cmds.is_some());
    assert!(task.env.is_some());
    assert_eq!(task.dir, Some(PathBuf::from("./test")));
    assert_eq!(task.platforms, Some(vec!["linux".to_string(), "macos".to_string()]));
    assert_eq!(task.ignore_error, Some(true));
    assert!(task.deps.is_some());
    assert_eq!(task.silent, Some(true));
  }

  #[test]
  fn parses_octafile_and_task_dotenv() {
    let content = r#"
      version: 1
      dotenv:
        - .env.local
        - config/base.env
      tasks:
        test:
          dotenv:
            - .env.test
          shell: echo test
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "dotenv");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    assert_eq!(
      octafile.dotenv,
      Some(vec![".env.local".to_string(), "config/base.env".to_string()])
    );
    assert_eq!(octafile.tasks["test"].dotenv, Some(vec![".env.test".to_string()]));
  }

  #[test]
  fn parses_shell_backed_environment_values() {
    let content = r#"
      version: 1
      env:
        STATIC: value
        DYNAMIC:
          sh: echo dynamic
      tasks:
        test:
          env:
            TASK_DYNAMIC:
              sh: echo task
          shell: echo test
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "shell_env");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    assert_eq!(octafile.env.as_ref().unwrap()["STATIC"], EnvValue::from("value"));
    assert_eq!(
      octafile.env.as_ref().unwrap()["DYNAMIC"],
      EnvValue::Shell(ShellValue {
        sh: "echo dynamic".to_owned()
      })
    );
    assert!(matches!(
      octafile.tasks["test"].env.as_ref().unwrap()["TASK_DYNAMIC"],
      EnvValue::Shell(_)
    ));
  }

  #[test]
  fn rejects_invalid_shell_backed_environment_values() {
    let content = r#"
      version: 1
      env:
        INVALID:
          sh: echo invalid
          extra: value
      tasks:
        test: echo test
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_shell_env");

    assert!(Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").is_err());
  }

  #[test]
  fn test_rejects_multiple_plugin_task_types() {
    let content = r#"
      version: 1
      tasks:
        plugin_task:
          plugin_key: plugin value
          another_key: another value
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "task_with_plugin_keys");

    let plugin_keys = vec!["plugin_key".to_string(), "another_key".to_string()];
    let error = Octafile::load(Some(file_path), false, plugin_keys, "plugin_key").unwrap_err();

    assert!(error.to_string().contains("more than one plugin task type"));
  }

  #[test]
  fn test_task_with_plugin_annotation() {
    let content = r#"
      version: 1
      tasks:
        build: !shell cargo build
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "task_with_plugin_annotation");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    let task = &octafile.tasks["build"];
    let plugin = task.plugin.as_ref().unwrap();
    assert_eq!(plugin.key, "shell");
    assert_eq!(plugin.value, Value::String("cargo build".to_string()));
  }

  #[test]
  fn test_task_annotation_supports_structured_plugin_params() {
    let content = r#"
      version: 1
      tasks:
        deploy: !docker
          image: app:latest
          command: ./deploy
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "task_annotation_structured_params");

    let octafile = Octafile::load(Some(file_path), false, vec!["docker".to_string()], "docker").unwrap();

    let plugin = octafile.tasks["deploy"].plugin.as_ref().unwrap();
    assert_eq!(plugin.key, "docker");
    let params = task_mapping(&plugin.value);
    assert_eq!(params["image"], Value::String("app:latest".to_string()));
    assert_eq!(params["command"], Value::String("./deploy".to_string()));
  }

  #[test]
  fn test_unknown_task_annotation_is_an_error() {
    let content = r#"
      version: 1
      tasks:
        build: !missing cargo build
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "unknown_task_annotation");

    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();
    let message = error.to_string();

    assert!(message.contains("unknown task annotation '!missing'"));
    assert!(message.contains("line"));
    assert!(message.contains("column"));
  }

  fn docker_schemas() -> PluginSchemas {
    PluginSchemas::from([(
      "docker".to_string(),
      serde_json::json!({
        "type": "object",
        "properties": {
          "image": { "type": "string" },
          "replicas": { "type": "integer", "minimum": 1 }
        },
        "required": ["image"],
        "additionalProperties": false
      })
      .as_object()
      .cloned(),
    )])
  }

  #[test]
  fn validates_plugin_params_in_tasks_and_commands() {
    let content = r#"
      version: 1
      tasks:
        deploy: !docker
          image: alpine
          replicas: 2
        pipeline:
          cmds:
            - docker:
                image: busybox
              platforms: [linux/amd64]
            - defer:
                docker:
                  image: alpine
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "valid_plugin_schema");

    let octafile = Octafile::load_with_schemas(Some(file_path), false, docker_schemas(), "docker").unwrap();

    assert_eq!(octafile.tasks["deploy"].plugin.as_ref().unwrap().key, "docker");
    let command = &octafile.tasks["pipeline"].cmds.as_ref().unwrap()[0];
    assert_eq!(command.options.platforms, Some(vec!["linux/amd64".to_string()]));
    assert!(matches!(
      &command.payload,
      CommandPayload::Plugin(plugin) if plugin.key == "docker"
    ));
    assert!(octafile.tasks["pipeline"].cmds.as_ref().unwrap()[1].options.deferred);
  }

  #[test]
  fn validates_plugin_conditions() {
    let content = r#"
      version: 1
      tasks:
        guarded:
          if:
            docker:
              image: condition
          docker:
            image: task
        pipeline:
          cmds:
            - docker:
                image: command
              if:
                docker:
                  image: command-condition
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "plugin_conditions");

    let octafile = Octafile::load_with_schemas(Some(file_path), false, docker_schemas(), "docker").unwrap();
    let task_condition = octafile.tasks["guarded"]
      .condition
      .as_ref()
      .and_then(|conditions| conditions.after_deps.as_ref())
      .unwrap();
    assert_eq!(task_condition.command.key, "docker");
    assert_eq!(task_condition.command.value["image"].as_str(), Some("condition"));

    let command_condition = octafile.tasks["pipeline"].cmds.as_ref().unwrap()[0]
      .options
      .condition
      .as_ref()
      .unwrap();
    assert_eq!(command_condition.key, "docker");
    assert_eq!(command_condition.value["image"].as_str(), Some("command-condition"));
  }

  #[test]
  fn rejects_invalid_plugin_conditions() {
    let content = r#"
      version: 1
      tasks:
        guarded:
          if:
            after_deps:
              docker:
                replicas: 0
          docker:
            image: task
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_plugin_condition");

    let error = Octafile::load_with_schemas(Some(file_path), false, docker_schemas(), "docker").unwrap_err();

    assert!(error.to_string().contains("invalid parameters for plugin 'docker'"));
  }

  #[test]
  fn parses_platforms_on_task_reference_commands() {
    let content = r#"
      version: 1
      tasks:
        pipeline:
          cmds:
            - task: build
              vars:
                profile: release
              platforms: [darwin, linux/arm64]
        build:
          shell: echo build
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "command_platforms");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();
    let command = &octafile.tasks["pipeline"].cmds.as_ref().unwrap()[0];

    assert_eq!(
      command.options.platforms,
      Some(vec!["darwin".to_string(), "linux/arm64".to_string()])
    );
    assert!(matches!(
      &command.payload,
      CommandPayload::Task(dependency) if dependency.task == "build"
    ));
  }

  #[test]
  fn rejects_invalid_command_platforms() {
    let content = r#"
      version: 1
      tasks:
        build:
          cmds:
            - shell: echo build
              platforms: linux
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_command_platforms");

    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();

    assert!(error.to_string().contains("invalid command platforms"));
  }

  #[test]
  fn parses_task_command_and_dependency_timeouts() {
    let content = r#"
      version: 1
      tasks:
        pipeline:
          timeout: 2m
          deps:
            - task: prepare
              timeout: 30s
          cmds:
            - shell: echo build
              timeout: 1500ms
        prepare:
          shell: echo prepare
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "timeouts");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();
    let task = &octafile.tasks["pipeline"];

    assert_eq!(task.timeout.unwrap().duration(), std::time::Duration::from_secs(120));
    assert_eq!(
      task.cmds.as_ref().unwrap()[0].options.timeout.unwrap().duration(),
      std::time::Duration::from_millis(1500)
    );
    let Deps::Complex(dependency) = &task.deps.as_ref().unwrap()[0] else {
      panic!("expected a complex dependency");
    };
    assert_eq!(
      dependency.timeout.unwrap().duration(),
      std::time::Duration::from_secs(30)
    );
  }

  #[test]
  fn parses_command_execution_options() {
    let content = r#"
      version: 1
      tasks:
        pipeline:
          cmds:
            - shell: echo build
              if: test -f Cargo.toml
              silent: true
              ignore_error: true
            - task: cleanup
              if: test -d target
              silent: false
              ignore_error: false
        cleanup:
          shell: echo cleanup
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "command_options");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();
    let commands = octafile.tasks["pipeline"].cmds.as_ref().unwrap();

    assert_eq!(
      commands[0].options.condition,
      Some(shell_condition("test -f Cargo.toml"))
    );
    assert_eq!(commands[0].options.silent, Some(true));
    assert_eq!(commands[0].options.ignore_error, Some(true));
    assert_eq!(commands[1].options.condition, Some(shell_condition("test -d target")));
    assert_eq!(commands[1].options.silent, Some(false));
    assert_eq!(commands[1].options.ignore_error, Some(false));
  }

  #[test]
  fn rejects_invalid_command_execution_options() {
    for (field, value, expected) in [
      ("if", "true", "condition"),
      ("silent", "not-a-boolean", "silent"),
      ("ignore_error", "not-a-boolean", "ignore_error"),
    ] {
      let content = format!(
        r#"
          version: 1
          tasks:
            pipeline:
              cmds:
                - shell: echo build
                  {field}: {value}
        "#
      );
      let (_temp_dir, file_path) = create_temp_octafile(&content, "invalid_command_options");

      let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();

      assert!(error.to_string().contains(expected));
    }
  }

  #[test]
  fn rejects_invalid_and_zero_timeouts() {
    for timeout in ["later", "0s"] {
      let content = format!(
        r#"
          version: 1
          tasks:
            build:
              timeout: {timeout}
              shell: echo build
        "#
      );
      let (_temp_dir, file_path) = create_temp_octafile(&content, "invalid_timeout");

      let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();

      assert!(error.to_string().contains("timeout"));
    }
  }

  #[test]
  fn parses_deferred_commands() {
    let content = r#"
      version: 1
      tasks:
        pipeline:
          cmds:
            - defer: echo cleanup
            - defer:
                task: cleanup
              platforms: [linux]
              if: test -d target
              silent: true
              ignore_error: true
        cleanup:
          shell: echo cleanup task
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "deferred_commands");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();
    let commands = octafile.tasks["pipeline"].cmds.as_ref().unwrap();

    assert!(commands[0].options.deferred);
    assert!(matches!(
      &commands[0].payload,
      CommandPayload::Plugin(plugin)
        if plugin.key == "shell" && plugin.value.as_str() == Some("echo cleanup")
    ));
    assert!(commands[1].options.deferred);
    assert_eq!(commands[1].options.platforms, Some(vec!["linux".to_string()]));
    assert_eq!(commands[1].options.condition, Some(shell_condition("test -d target")));
    assert_eq!(commands[1].options.silent, Some(true));
    assert_eq!(commands[1].options.ignore_error, Some(true));
    assert!(matches!(
      &commands[1].payload,
      CommandPayload::Task(dependency) if dependency.task == "cleanup"
    ));
  }

  #[test]
  fn rejects_deferred_command_with_sibling_command() {
    let content = r#"
      version: 1
      tasks:
        pipeline:
          cmds:
            - defer: echo cleanup
              shell: echo work
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_deferred_command");

    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();

    assert!(error.to_string().contains("deferred command cannot contain sibling"));
  }

  #[test]
  fn rejects_invalid_annotated_plugin_params() {
    let content = r#"
      version: 1
      tasks:
        deploy: !docker
          image: 42
          replicas: 0
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_plugin_annotation");

    let error = Octafile::load_with_schemas(Some(file_path), false, docker_schemas(), "docker").unwrap_err();
    let message = error.to_string();

    assert!(message.contains("invalid parameters for plugin 'docker'"));
    assert!(message.contains("line"));
  }

  #[test]
  fn rejects_invalid_plugin_command_params() {
    let content = r#"
      version: 1
      tasks:
        deploy:
          cmds:
            - docker:
                replicas: 2
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_plugin_command");

    let error = Octafile::load_with_schemas(Some(file_path), false, docker_schemas(), "docker").unwrap_err();

    assert!(error.to_string().contains("invalid parameters for plugin 'docker'"));
  }

  #[test]
  fn validates_plugin_params_in_included_octafiles() {
    let root_content = r#"
      version: 1
      includes:
        child: child.yml
    "#;
    let child_content = r#"
      version: 1
      tasks:
        deploy:
          docker:
            image: 42
    "#;
    let temp_dir = Builder::new().prefix("included_plugin_schema").tempdir().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");
    fs::write(&root_path, root_content).unwrap();
    fs::write(temp_dir.path().join("child.yml"), child_content).unwrap();

    let error = Octafile::load_with_schemas(Some(root_path), false, docker_schemas(), "docker").unwrap_err();

    assert!(error.to_string().contains("invalid parameters for plugin 'docker'"));
    assert!(error.to_string().contains("child.yml"));
  }

  #[test]
  fn rejects_invalid_plugin_validation_schema() {
    let content = "version: 1\n";
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_plugin_schema");
    let schemas = PluginSchemas::from([(
      "docker".to_string(),
      serde_json::json!({ "type": "not-a-json-schema-type" })
        .as_object()
        .cloned(),
    )]);

    let error = Octafile::load_with_schemas(Some(file_path), false, schemas, "docker").unwrap_err();

    assert!(matches!(error, OctafileError::PluginSchemaError(_)));
  }

  #[test]
  fn rejects_unknown_octafile_and_task_fields() {
    let content = r#"
      version: 1
      taskz: {}
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "unknown_octafile_field");
    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();
    assert!(error.to_string().contains("unknown Octafile field 'taskz'"));

    let content = r#"
      version: 1
      tasks:
        build:
          slient: true
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "unknown_task_field");
    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap_err();
    assert!(error.to_string().contains("unknown task field 'slient'"));
  }

  #[test]
  fn test_standard_yaml_tag_is_not_a_task_annotation() {
    let content = r#"
      version: 1
      tasks:
        build: !!str cargo build
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "standard_yaml_tag");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    let plugin = octafile.tasks["build"].plugin.as_ref().unwrap();
    assert_eq!(plugin.key, "shell");
    assert_eq!(plugin.value, Value::String("cargo build".to_string()));
  }

  #[test]
  fn test_task_with_all_optional_fields() {
    let content = r#"
      version: 1
      tasks:
        full_task:
          desc: "Full task description"
          cmds:
            - echo "step 1"
            - echo "step 2"
          env:
            TEST_VAR: "test value"
          vars:
            task_var: "task value"
          dir: "./test"
          internal: true
          platforms:
            - linux
            - macos
          ignore_error: true
          deps:
            - other_task
          run: once
          silent: true
          execute_mode: parallel
          timeout: 2m
          sources:
            - "src/**/*.rs"
          source_strategy: hash
          watch: true
          if: test -f "Cargo.toml"
          preconditions:
            - test -f "file.txt"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "task_with_all_fields");
    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    let task = &octafile.tasks["full_task"];
    assert_eq!(task.desc, Some("Full task description".to_string()));
    assert!(task.cmds.is_some());
    assert!(task.env.is_some());
    assert!(task.vars.is_some());
    assert_eq!(task.dir, Some(PathBuf::from("./test")));
    assert_eq!(task.internal, Some(true));
    assert_eq!(task.platforms, Some(vec!["linux".to_string(), "macos".to_string()]));
    assert_eq!(task.ignore_error, Some(true));
    assert!(task.deps.is_some());
    assert!(task.run.is_some());
    assert_eq!(task.silent, Some(true));
    assert!(task.execute_mode.is_some());
    assert_eq!(task.timeout.unwrap().duration(), std::time::Duration::from_secs(120));
    assert!(task.sources.is_some());
    assert!(task.source_strategy.is_some());
    assert_eq!(task.watch, Some(true));
    assert_eq!(
      task
        .condition
        .as_ref()
        .and_then(|condition| condition.after_deps.as_ref()),
      Some(&TaskCondition {
        command: shell_condition("test -f \"Cargo.toml\""),
        evaluate: ConditionEvaluation::Once,
      })
    );
    assert!(task.preconditions.is_some());
  }

  #[test]
  fn test_invalid_task_values() {
    // Test invalid run value
    let content = r#"
      version: 1
      tasks:
        invalid_task:
          run: invalid_value
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_task_values");
    assert!(Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").is_err());

    // Test invalid execute_mode value
    let content = r#"
      version: 1
      tasks:
        invalid_task:
          execute_mode: invalid_mode
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_execute_mode");
    assert!(Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").is_err());

    let content = r#"
      version: 1
      tasks:
        invalid_task:
          if: true
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_condition");
    assert!(Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").is_err());
  }

  #[test]
  fn test_phased_task_conditions() {
    let content = r#"
      version: 1
      tasks:
        deploy:
          if:
            before_deps: test -f config.yml
            after_deps:
              tpl: deployment-ready
              evaluate: per_command
          cmds:
            - echo prepare
            - echo deploy
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "phased_conditions");
    let octafile = Octafile::load(
      Some(file_path),
      false,
      vec!["shell".to_string(), "tpl".to_string()],
      "shell",
    )
    .unwrap();
    let condition = octafile.tasks["deploy"].condition.as_ref().unwrap();

    assert_eq!(
      condition.before_deps.as_ref(),
      Some(&TaskCondition {
        command: shell_condition("test -f config.yml"),
        evaluate: ConditionEvaluation::Once,
      })
    );
    assert_eq!(
      condition.after_deps.as_ref(),
      Some(&TaskCondition {
        command: PluginCommand {
          key: "tpl".to_string(),
          value: Value::String("deployment-ready".to_string()),
        },
        evaluate: ConditionEvaluation::PerCommand,
      })
    );
  }

  #[test]
  fn test_rejects_invalid_phased_task_conditions() {
    for (name, condition) in [
      ("empty", "if: {}"),
      (
        "before_deps_per_command",
        "if:\n            before_deps:\n              shell: test -f config.yml\n              evaluate: per_command",
      ),
    ] {
      let content = format!("version: 1\ntasks:\n  invalid:\n    {condition}\n    shell: echo invalid\n");
      let (_temp_dir, file_path) = create_temp_octafile(&content, name);

      assert!(Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").is_err());
    }
  }

  #[test]
  fn test_task_deps_variations() {
    let content = r#"
      version: 1
      tasks:
        task_with_deps:
          cmds:
            - echo "main task"
          deps:
            - simple_dep
            - task: complex_dep
              vars:
                key: value
              envs:
                ENV_VAR: value
              silent: true
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "task_deps_variations");
    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    let task = &octafile.tasks["task_with_deps"];
    assert!(task.deps.is_some());
    let deps = task.deps.as_ref().unwrap();
    assert_eq!(deps.len(), 2);
  }

  #[test]
  fn test_empty_octafile() {
    let content = r#"
        version: 1
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "empty_octafile");
    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    assert_eq!(octafile.version, Version::V1 as u8);
    assert!(octafile.tasks.is_empty());
    assert!(octafile.includes.is_none());
    assert!(octafile.vars.is_none());
    assert!(octafile.env.is_none());
  }

  #[test]
  fn parses_ordered_and_secret_variables() {
    let vars: Vars = serde_yml::from_str(
      r#"
      FIRST: value
      TOKEN:
        value: secret-value
        secret: true
      DYNAMIC:
        sh: echo dynamic
        secret: true
      "#,
    )
    .unwrap();

    assert_eq!(
      vars.keys().map(String::as_str).collect::<Vec<_>>(),
      ["FIRST", "TOKEN", "DYNAMIC"]
    );
    assert!(vars["TOKEN"].is_secret());
    assert!(!format!("{:?}", vars["TOKEN"]).contains("secret-value"));
    assert_eq!(
      vars["TOKEN"].clone().into_value(),
      serde_json::Value::String("secret-value".to_owned())
    );
    assert!(vars["DYNAMIC"].is_secret());
    assert_eq!(
      vars["DYNAMIC"].clone().into_value(),
      serde_json::json!({ "sh": "echo dynamic" })
    );

    let serialized = serde_yml::to_string(&vars).unwrap();
    assert_eq!(serde_yml::from_str::<Vars>(&serialized).unwrap(), vars);
  }

  #[test]
  fn rejects_invalid_secret_variables_during_parsing() {
    for definition in [
      "value: hidden\n    sh: echo hidden\n    secret: true",
      "value: hidden\n    secret: yes",
      "value: hidden\n    secret: true\n    unknown: value",
    ] {
      let yaml = format!("TOKEN:\n    {definition}\n");
      assert!(serde_yml::from_str::<Vars>(&yaml).is_err(), "accepted {definition}");
    }
  }

  #[test]
  fn parses_failfast_at_octafile_and_task_levels() {
    let content = r#"
      version: 1
      failfast: true
      tasks:
        inherited:
          shell: echo inherited
        overridden:
          failfast: false
          shell: echo overridden
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "failfast");

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    assert_eq!(octafile.failfast, Some(true));
    assert_eq!(octafile.tasks["inherited"].failfast, None);
    assert_eq!(octafile.tasks["overridden"].failfast, Some(false));
  }
}
