use std::{
  collections::HashMap,
  env,
  fs::File,
  io::Read,
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{OctafileError, OctafileResult};

const OCTAFILE_DEFAULT_NAMES: [&str; 4] = ["Octafile.yml", "octafile.yml", "Octafile.yaml", "octafile.yaml"];

/// Represents include directive information in taskfile
#[derive(Serialize, Deserialize, Debug)]
pub struct IncludeInfo {
  octafile: String,
  optional: Option<bool>,
}

type Vars = HashMap<String, String>;

type Envs = HashMap<String, String>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Task {
  pub dir: Option<PathBuf>,                  // Working directory for the task
  pub vars: Option<HashMap<String, String>>, // Task-specific variables
  pub cmd: Option<String>,                   // Command to execute
  pub cmds: Option<Vec<String>>,             // List of commands
  pub platforms: Option<Vec<String>>,        // Supported platforms
  pub ignore_error: Option<bool>,            // Whether to continue on error
  pub deps: Option<Vec<String>>,             // Task dependencies
}

/// Main taskfile structure representing the entire configuration
#[derive(Serialize, Deserialize, Debug)]
pub struct Octafile {
  // Octafile schema version
  pub version: u32,

  // Name of octafile. Using for debug
  pub name: Option<String>,

  // Taskfile global vars
  pub vars: Option<Vars>,

  // Taskfile global environment variables
  pub env: Option<Envs>,

  // list of included octafiles
  pub includes: Option<HashMap<String, IncludeInfo>>,

  // List of task
  pub tasks: HashMap<String, Task>,

  // Working directory for the octafile
  #[serde(skip_deserializing, skip_serializing)]
  pub dir: PathBuf,

  // Internal list of octafiles
  #[serde(skip_deserializing, skip_serializing)]
  _included: Mutex<HashMap<String, Arc<Octafile>>>,

  // Parent octafile
  #[serde(skip_deserializing, skip_serializing)]
  _parent: Option<Arc<Octafile>>,
}

impl Octafile {
  pub fn load(path: Option<PathBuf>) -> OctafileResult<Arc<Self>> {
    let path = match path {
      Some(path) => path,
      None => Octafile::find_octafile()?.ok_or(OctafileError::NotSearchedError)?,
    };
    let path = path.canonicalize().map_err(OctafileError::IoError)?;

    debug!("Loading octafile: {}", path.display());

    let mut octafile = Self::read_octafile(&path)?;
    octafile.set_attributes(&path)?;

    let octafile = Arc::new(octafile);
    Self::load_includes(Arc::clone(&octafile))?;

    Ok(octafile)
  }

  fn load_includes(octafile: Arc<Octafile>) -> OctafileResult<()> {
    let includes = match &octafile.includes {
      Some(includes) => includes,
      None => return Ok(()),
    };

    for (name, include) in includes {
      let path = octafile
        .dir
        .join(&include.octafile)
        .canonicalize()
        .map_err(OctafileError::IoError)?;

      debug!("Loading included octafile: {}", path.display());
      let mut include_octafile = match Self::read_octafile(&path) {
        Ok(mut t) => {
          t._parent = Some(Arc::clone(&octafile));
          t
        },
        Err(OctafileError::NotFoundError(e)) => {
          if include.optional.unwrap_or(false) {
            continue;
          }
          return Err(OctafileError::NotFoundError(e));
        },
        Err(e) => return Err(e),
      };

      include_octafile.set_attributes(&path)?;
      let include_octafile = Arc::new(include_octafile);

      // Recursively process nested includes
      if include_octafile.includes.is_some() {
        Self::load_includes(Arc::clone(&include_octafile))?;
      }

      octafile
        ._included
        .lock()
        .map_err(|_| OctafileError::LockError("Failed to lock included octafiles".to_string()))?
        .insert(name.clone(), include_octafile);
    }

    Ok(())
  }

  /// Filters tasks based on the current platform
  fn filter_task_by_platform(tasks: HashMap<String, Task>) -> HashMap<String, Task> {
    let os_type = sys_info::os_type().map(|os| os.to_lowercase()).unwrap_or_default();

    tasks
      .into_iter()
      .filter(|(_, cmd)| {
        cmd
          .platforms
          .as_ref()
          .map_or(true, |platforms| platforms.contains(&os_type))
      })
      .collect()
  }

  /// Sets common attributes for an Octafile, including merging from parent if present
  fn set_attributes(&mut self, path: &Path) -> OctafileResult<()> {
    // Process tasks
    self.tasks = Self::filter_task_by_platform(self.tasks.clone());

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
  fn read_octafile<P: AsRef<Path>>(taskfile_path: P) -> OctafileResult<Octafile> {
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

    serde_yml::from_str(&content).map_err(|e| OctafileError::ParseError(path_str, e.to_string()))
  }

  /// Try to find octafile config traversing to root directory from current directory
  fn find_octafile() -> OctafileResult<Option<PathBuf>> {
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

    Ok(None)
  }

  pub fn get_included(&self, name: &str) -> OctafileResult<Option<Arc<Octafile>>> {
    self
      ._included
      .lock()
      .map_err(|_| OctafileError::LockError("Failed to lock included taskfiles".to_string()))
      .map(|guard| guard.get(name).map(Arc::clone))
  }

  pub fn get_all_included(&self) -> OctafileResult<HashMap<String, Arc<Octafile>>> {
    self
      ._included
      .lock()
      .map_err(|e| OctafileError::LockError(format!("Failed to lock included taskfiles: {}", e)))
      .map(|guard| {
        guard
          .iter()
          .map(|(k, v)| (k.clone(), Arc::clone(v)))
          .collect::<HashMap<_, _>>()
      })
  }

  pub fn parent(&self) -> Option<&Arc<Octafile>> {
    self._parent.as_ref()
  }

  pub fn root(&self) -> &Octafile {
    let mut current = self;
    while let Some(parent) = current.parent() {
      current = parent;
    }
    current
  }

  pub fn hierarchy_path(&self) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = self;

    while let Some(parent) = current.parent() {
      if let Some(name) = &current.name {
        path.push(name.clone());
      }
      current = parent;
    }

    if let Some(name) = &current.name {
      path.push(name.clone());
    }

    path.reverse();
    path
  }
}
