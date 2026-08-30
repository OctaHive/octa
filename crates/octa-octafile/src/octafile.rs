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

use serde::{
  de::{DeserializeSeed, MapAccess, Visitor},
  Deserialize, Deserializer, Serialize,
};
use serde_yml::Value;
use tracing::{debug, info};

use crate::{
  error::{OctafileError, OctafileResult},
  include::IncludeInfo,
  parser::{self, location_error, Node},
  task::{AllowedRun, Context, PluginSchemas, Task, TaskSeed},
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

pub type Vars = HashMap<String, Value>;
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

  // Watch polling interval
  pub interval: Option<WatchInterval>,

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
      .field("interval", &self.interval)
      .field("includes", &self.includes)
      .field("tasks", &self.tasks)
      .field("dir", &self.dir)
      .finish()
  }
}

impl Octafile {
  pub fn load(path: Option<PathBuf>, global: bool, plugin_keys: Vec<String>) -> OctafileResult<Arc<Self>> {
    Self::load_with_context(path, global, Context::from_keys(plugin_keys))
  }

  pub fn load_with_schemas(path: Option<PathBuf>, global: bool, schemas: PluginSchemas) -> OctafileResult<Arc<Self>> {
    let context = Context::from_schemas(schemas).map_err(OctafileError::PluginSchemaError)?;
    Self::load_with_context(path, global, context)
  }

  fn load_with_context(path: Option<PathBuf>, global: bool, context: Context) -> OctafileResult<Arc<Self>> {
    let path = match path {
      Some(path) => Octafile::find_octafile(Some(path)),
      None => {
        if global {
          let home = dirs::home_dir();

          if let Some(home) = home {
            Ok(Some(home))
          } else {
            return Err(OctafileError::NotSearchedError);
          }
        } else {
          Octafile::find_octafile(None)
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
    let mut octafile = Octafile::default();
    let mut fields = HashSet::new();

    for (key, value) in map {
      let key_marker = key.marker();
      let key = key
        .as_str()
        .ok_or_else(|| location_error(key_marker, "Octafile keys must be strings"))?;

      if !fields.insert(key.to_owned()) {
        return Err(location_error(key_marker, &format!("duplicated key '{key}'")));
      }

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
        "interval" => {
          octafile.interval = serde_yml::from_value(value.into_value()?).map_err(|e| e.to_string())?;
        },
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

              let mut extra = HashMap::new();
              extra.insert(annotation, value);
              Task {
                extra,
                ..Task::default()
              }
            } else {
              match task_value.into_value()? {
                Value::String(value) => {
                  let task_visitor = crate::task::TaskVisitor { context };
                  task_visitor
                    .visit_str::<serde_yml::Error>(&value)
                    .map_err(|e| e.to_string())?
                },
                task_value => {
                  let task_seed = TaskSeed { context };
                  let deserializer = serde_yml::Deserializer::new(&task_value);
                  task_seed.deserialize(deserializer).map_err(|e| e.to_string())?
                },
              }
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

    for (name, include) in includes {
      let path = match include {
        IncludeInfo::Simple(path) => match octafile.dir.join(path).canonicalize() {
          Ok(path) => {
            Octafile::find_octafile(Some(path.clone()))?.ok_or(OctafileError::NotFoundError(path.display().to_string()))
          },
          Err(_) => Err(OctafileError::NotFoundError(path.clone())),
        }?,
        IncludeInfo::Complex(complex) => match octafile.dir.join(&complex.octafile).canonicalize() {
          Ok(path) => {
            Octafile::find_octafile(Some(path))?.ok_or(OctafileError::NotFoundError(complex.octafile.clone()))?
          },
          Err(_) => {
            if let Some(optional) = complex.optional {
              if optional {
                continue;
              }
            }

            return Err(OctafileError::NotFoundError(complex.octafile.clone()));
          },
        },
      };

      debug!("Loading included octafile: {}", path.display());
      let mut include_octafile = match Self::read_octafile(&path, context) {
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
        Self::load_includes(Arc::clone(&include_octafile), context)?;
      }

      octafile
        ._included
        .lock()
        .map_err(|_| OctafileError::LockError("Failed to lock included octafiles".to_string()))?
        .insert(name.clone(), include_octafile);
    }

    Ok(())
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
          if potential_path.exists() {
            return Ok(Some(potential_path));
          }
        }
      } else {
        return Ok(Some(path));
      }
    } else {
      let mut current_dir = env::current_dir()?;
      loop {
        for taskfile_name in OCTAFILE_DEFAULT_NAMES {
          let potential_path = current_dir.join(taskfile_name);
          if potential_path.exists() {
            return Ok(Some(potential_path));
          }
        }

        if let Some(parent) = current_dir.parent() {
          current_dir = parent.to_path_buf();
        } else {
          break;
        }
      }
    }

    Ok(None)
  }
}

impl<'de> Deserialize<'de> for Octafile {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    // Create a default context for regular deserialization
    let context = Context::default();
    let visitor = OctafileVisitor { context };
    deserializer.deserialize_map(visitor)
  }
}

struct OctafileVisitor {
  context: Context,
}

impl<'de> Visitor<'de> for OctafileVisitor {
  type Value = Octafile;

  fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
    formatter.write_str("a map with Octafile fields")
  }

  fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
  where
    A: MapAccess<'de>,
  {
    let mut octafile = Octafile::default();

    while let Some(key) = map.next_key::<String>()? {
      match key.as_str() {
        "version" => {
          octafile.version = map.next_value()?;
        },
        "vars" => {
          octafile.vars = map.next_value()?;
        },
        "env" => {
          octafile.env = map.next_value()?;
        },
        "dotenv" => {
          octafile.dotenv = map.next_value()?;
        },
        "run" => {
          octafile.run = map.next_value()?;
        },
        "interval" => {
          octafile.interval = map.next_value()?;
        },
        "includes" => {
          octafile.includes = map.next_value()?;
        },
        "tasks" => {
          if let Value::Mapping(tasks_map) = map.next_value::<Value>()? {
            let mut tasks = HashMap::new();
            for (task_key, task_value) in tasks_map {
              let task = match task_value {
                Value::String(s) => {
                  let task_visitor = crate::task::TaskVisitor { context: &self.context };
                  task_visitor
                    .visit_str::<A::Error>(&s)
                    .map_err(serde::de::Error::custom)?
                },
                task_value => {
                  let task_seed = TaskSeed { context: &self.context };
                  let deserializer = serde_yml::Deserializer::new(&task_value);
                  task_seed.deserialize(deserializer).map_err(serde::de::Error::custom)?
                },
              };

              tasks.insert(task_key, task);
            }
            octafile.tasks = tasks;
          }
        },
        _ => return Err(serde::de::Error::custom(format!("unknown Octafile field '{key}'"))),
      }
    }

    Ok(octafile)
  }
}

#[cfg(test)]
mod tests {
  use super::OCTAFILE_DEFAULT_NAMES;
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

    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();
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

    assert!(Octafile::load(Some(file_path), false, vec![]).is_err());
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

      assert!(Octafile::load(Some(file_path), false, vec![]).is_err());
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

    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();
    assert_eq!(octafile.version, 1);

    // Test string task
    let simple_task = &octafile.tasks["simple_string"];
    assert!(simple_task.extra.contains_key("shell"));
    assert_eq!(simple_task.extra["shell"], Value::String("echo \"simple\"".to_string()));

    // Test complex task
    let complex_task = &octafile.tasks["complex_map"];
    assert_eq!(complex_task.desc, Some("A complex task".to_string()));
    assert!(complex_task.cmds.is_some());

    // Test another string task
    let another_task = &octafile.tasks["another_string"];
    assert!(another_task.extra.contains_key("shell"));
    assert_eq!(
      another_task.extra["shell"],
      Value::String("echo \"another\"".to_string())
    );
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

    let root = Octafile::load(Some(root_path), false, vec![]).unwrap();

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

    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();
    assert!(octafile.get_included("optional").unwrap().is_none());
  }

  #[test]
  fn test_error_handling() {
    // Test nonexistent file
    assert!(matches!(
      Octafile::load(Some(PathBuf::from("nonexistent.yml")), false, vec![]),
      Err(OctafileError::IoError(_))
    ));

    // Test invalid YAML
    let content = "invalid: : yaml:";
    let (_temp_dir, file_path) = create_temp_octafile(content, "error_handling");
    assert!(matches!(
      Octafile::load(Some(file_path), false, vec![]),
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
      Octafile::load(Some(file_path), false, vec![]),
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

    let octafile = Octafile::load(Some(file_path.clone()), false, vec![]).unwrap();
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

    let root = Octafile::load(Some(root_path), false, vec![]).unwrap();
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
    let root = Octafile::load(Some(root_path), false, vec![]).unwrap();

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

    let root = Octafile::load(Some(root_path), false, vec![]).unwrap();

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
    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();

    let task = &octafile.tasks["simple"];
    assert!(task.extra.contains_key("shell"));
    assert_eq!(task.extra["shell"], Value::String("echo \"test\"".to_string()));
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
    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();

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

    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();

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

    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();

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

    assert!(Octafile::load(Some(file_path), false, vec![]).is_err());
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
    let error = Octafile::load(Some(file_path), false, plugin_keys).unwrap_err();

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

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()]).unwrap();

    let task = &octafile.tasks["build"];
    assert_eq!(task.extra["shell"], Value::String("cargo build".to_string()));
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

    let octafile = Octafile::load(Some(file_path), false, vec!["docker".to_string()]).unwrap();

    let params = task_mapping(&octafile.tasks["deploy"].extra["docker"]);
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

    let error = Octafile::load(Some(file_path), false, vec!["shell".to_string()]).unwrap_err();
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

    let octafile = Octafile::load_with_schemas(Some(file_path), false, docker_schemas()).unwrap();

    assert!(octafile.tasks["deploy"].extra.contains_key("docker"));
    let command = &octafile.tasks["pipeline"].cmds.as_ref().unwrap()[0];
    assert_eq!(command.platforms, Some(vec!["linux/amd64".to_string()]));
    assert!(!command.value.as_mapping().unwrap().contains_key("platforms"));
    assert!(octafile.tasks["pipeline"].cmds.as_ref().unwrap()[1].deferred);
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

    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();
    let command = &octafile.tasks["pipeline"].cmds.as_ref().unwrap()[0];

    assert_eq!(
      command.platforms,
      Some(vec!["darwin".to_string(), "linux/arm64".to_string()])
    );
    assert!(!command.value.as_mapping().unwrap().contains_key("platforms"));
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

    let error = Octafile::load(Some(file_path), false, vec![]).unwrap_err();

    assert!(error.to_string().contains("invalid command platforms"));
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
        cleanup:
          shell: echo cleanup task
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "deferred_commands");

    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();
    let commands = octafile.tasks["pipeline"].cmds.as_ref().unwrap();

    assert!(commands[0].deferred);
    assert_eq!(commands[0].value, Value::String("echo cleanup".to_string()));
    assert!(commands[1].deferred);
    assert_eq!(commands[1].platforms, Some(vec!["linux".to_string()]));
    assert_eq!(
      commands[1].value.as_mapping().unwrap()["task"].as_str(),
      Some("cleanup")
    );
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

    let error = Octafile::load(Some(file_path), false, vec![]).unwrap_err();

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

    let error = Octafile::load_with_schemas(Some(file_path), false, docker_schemas()).unwrap_err();
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

    let error = Octafile::load_with_schemas(Some(file_path), false, docker_schemas()).unwrap_err();

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

    let error = Octafile::load_with_schemas(Some(root_path), false, docker_schemas()).unwrap_err();

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

    let error = Octafile::load_with_schemas(Some(file_path), false, schemas).unwrap_err();

    assert!(matches!(error, OctafileError::PluginSchemaError(_)));
  }

  #[test]
  fn rejects_unknown_octafile_and_task_fields() {
    let content = r#"
      version: 1
      taskz: {}
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "unknown_octafile_field");
    let error = Octafile::load(Some(file_path), false, vec![]).unwrap_err();
    assert!(error.to_string().contains("unknown Octafile field 'taskz'"));

    let content = r#"
      version: 1
      tasks:
        build:
          slient: true
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "unknown_task_field");
    let error = Octafile::load(Some(file_path), false, vec![]).unwrap_err();
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

    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();

    assert_eq!(
      octafile.tasks["build"].extra["shell"],
      Value::String("cargo build".to_string())
    );
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
          sources:
            - "src/**/*.rs"
          source_strategy: hash
          watch: true
          if: test -f "Cargo.toml"
          preconditions:
            - test -f "file.txt"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "task_with_all_fields");
    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();

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
    assert!(task.sources.is_some());
    assert!(task.source_strategy.is_some());
    assert_eq!(task.watch, Some(true));
    assert_eq!(task.condition, Some("test -f \"Cargo.toml\"".to_string()));
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
    assert!(Octafile::load(Some(file_path), false, vec![]).is_err());

    // Test invalid execute_mode value
    let content = r#"
      version: 1
      tasks:
        invalid_task:
          execute_mode: invalid_mode
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_execute_mode");
    assert!(Octafile::load(Some(file_path), false, vec![]).is_err());

    let content = r#"
      version: 1
      tasks:
        invalid_task:
          if: true
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content, "invalid_condition");
    assert!(Octafile::load(Some(file_path), false, vec![]).is_err());
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
    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();

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
    let octafile = Octafile::load(Some(file_path), false, vec![]).unwrap();

    assert_eq!(octafile.version, Version::V1 as u8);
    assert!(octafile.tasks.is_empty());
    assert!(octafile.includes.is_none());
    assert!(octafile.vars.is_none());
    assert!(octafile.env.is_none());
  }
}
