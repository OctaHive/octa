use std::{
  collections::HashMap,
  env, fmt,
  fs::File,
  io::Read,
  path::{Path, PathBuf},
  sync::{Arc, Mutex, OnceLock},
};

use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::{
  error::{OctafileError, OctafileResult},
  include::IncludeInfo,
  task::Task,
};

const OCTAFILE_DEFAULT_NAMES: [&str; 4] = ["Octafile.yml", "octafile.yml", "Octafile.yaml", "octafile.yaml"];

type Vars = HashMap<String, String>;
type Envs = HashMap<String, String>;

/// Main taskfile structure representing the entire configuration
#[derive(Serialize, Deserialize)]
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
  #[serde(skip)]
  pub dir: PathBuf,

  // Internal list of octafiles
  #[serde(skip)]
  _included: Mutex<HashMap<String, Arc<Octafile>>>,

  // Parent octafile
  #[serde(skip)]
  _parent: Option<Arc<Octafile>>,

  // Self reference to octafile
  #[serde(skip)]
  _self: OnceLock<Arc<Octafile>>,
}

/// Custom Debug implementation to avoid cyclic reference
/// on self field
impl fmt::Debug for Octafile {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("Octafile")
      .field("version", &self.version)
      .field("name", &self.name)
      .field("includes", &self.includes)
      .field("tasks", &self.tasks)
      .field("dir", &self.dir)
      .finish()
  }
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
    octafile._self = OnceLock::new();

    let octafile = Arc::new(octafile);
    let _ = octafile._self.set(Arc::clone(&octafile));
    Self::load_includes(Arc::clone(&octafile))?;

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

  /// Return path from root octafile to current
  pub fn hierarchy_path(&self) -> Vec<String> {
    let mut path = Vec::new();
    let mut current = self;

    while let Some(parent) = current.parent() {
      if let Some(name) = &current.name {
        path.push(name.clone());
      }
      current = parent;
    }

    path.reverse();
    path
  }

  /// Load including octafiles
  fn load_includes(octafile: Arc<Octafile>) -> OctafileResult<()> {
    let includes = match &octafile.includes {
      Some(includes) => includes,
      None => return Ok(()),
    };

    for (name, include) in includes {
      let path = match octafile.dir.join(&include.octafile).canonicalize() {
        Ok(path) => Ok(path),
        Err(_) => {
          if let Some(optional) = include.optional {
            if optional {
              continue;
            }
          }

          Err(OctafileError::NotFoundError(include.octafile.clone()))
        },
      }?;

      debug!("Loading included octafile: {}", path.display());
      let mut include_octafile = match Self::read_octafile(&path) {
        Ok(mut t) => {
          t._parent = Some(Arc::clone(&octafile));
          t
        },
        Err(OctafileError::NotFoundError(e)) => {
          if include.optional.unwrap_or(false) {
            info!("Skipping optional {} octafile. Reason:: not found", path.display());

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
          .map_or(true, |platforms| platforms.contains(&os_type.clone().into()))
      })
      .collect()
  }

  /// Sets common attributes for an Octafile, including merging from parent if present
  fn set_attributes(&mut self, path: &Path) -> OctafileResult<()> {
    // Filter tasks by current platform
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
}

#[cfg(test)]
mod tests {
  use super::OCTAFILE_DEFAULT_NAMES;
  use crate::*;
  use pretty_assertions::assert_eq;
  use std::env;
  use std::fs;
  use std::path::PathBuf;
  use std::sync::Arc;
  use tempfile::TempDir;

  fn create_temp_octafile(content: &str) -> (TempDir, PathBuf) {
    let temp_dir = TempDir::new().unwrap();
    let file_path = temp_dir.path().join("Octafile.yml");
    fs::write(&file_path, content).unwrap();
    (temp_dir, file_path)
  }

  #[test]
  fn test_load_basic_octafile() {
    let content = r#"
      version: 1
      name: basic
      tasks:
        test:
          cmd: echo "hello"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content);

    let octafile = Octafile::load(Some(file_path)).unwrap();
    assert_eq!(octafile.version, 1);
    assert_eq!(octafile.name, Some("basic".to_string()));
    assert!(octafile.tasks.contains_key("test"));
  }

  #[test]
  fn test_nested_includes() {
    let root_content = r#"
      version: 1
      name: root
      includes:
        child:
          octafile: child/Octafile.yml
      tasks:
        root_task:
          cmd: echo "root"
    "#;

    let child_content = r#"
      version: 1
      name: child
      tasks:
        child_task:
          cmd: echo "child"
    "#;

    let temp_dir = TempDir::new().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");
    let child_dir = temp_dir.path().join("child");
    fs::create_dir(&child_dir).unwrap();
    let child_path = child_dir.join("Octafile.yml");

    fs::write(&root_path, root_content).unwrap();
    fs::write(&child_path, child_content).unwrap();

    let root = Octafile::load(Some(root_path)).unwrap();

    // Test basic structure
    assert_eq!(root.name, Some("root".to_string()));
    assert!(root.is_root());

    // Test includes
    let child = root.get_included("child").unwrap().unwrap();
    assert_eq!(child.name, Some("child".to_string()));
    assert!(!child.is_root());

    // Test hierarchy
    assert_eq!(child.hierarchy_path(), vec!["child".to_string()]);
  }

  #[test]
  fn test_optional_includes() {
    let content = r#"
      version: 1
      name: root
      includes:
        optional:
          octafile: nonexistent.yml
          optional: true
      tasks:
        root_task:
          cmd: echo "root"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content);

    let octafile = Octafile::load(Some(file_path)).unwrap();
    assert!(octafile.get_included("optional").unwrap().is_none());
  }

  #[test]
  fn test_platform_specific_tasks() {
    let content = format!(
      r#"
        version: 1
        name: test
        tasks:
          platform_task:
            cmd: echo "test"
            platforms: ["{}"]
          generic_task:
            cmd: echo "generic"
      "#,
      sys_info::os_type().unwrap().to_lowercase()
    );

    let (_temp_dir, file_path) = create_temp_octafile(&content);
    let octafile = Octafile::load(Some(file_path)).unwrap();

    assert!(octafile.tasks.contains_key("platform_task"));
    assert!(octafile.tasks.contains_key("generic_task"));
  }

  #[test]
  fn test_complex_commands() {
    let content = r#"
      version: 1
      name: test
      tasks:
        simple:
          cmd: echo "simple"
        complex:
          cmd:
            task: other_task
        multiple:
          cmds:
            - echo "first"
            - task: other_task
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content);

    let octafile = Octafile::load(Some(file_path)).unwrap();
    let tasks = &octafile.tasks;

    match &tasks["simple"].cmd {
      Some(Cmds::Simple(_)) => (),
      _ => panic!("Expected simple command"),
    }

    match &tasks["complex"].cmd {
      Some(Cmds::Complex(_)) => (),
      _ => panic!("Expected complex command"),
    }
  }

  #[test]
  fn test_error_handling() {
    // Test nonexistent file
    assert!(matches!(
      Octafile::load(Some(PathBuf::from("nonexistent.yml"))),
      Err(OctafileError::IoError(_))
    ));

    // Test invalid YAML
    let content = "invalid: : yaml:";
    let (_temp_dir, file_path) = create_temp_octafile(content);
    assert!(matches!(
      Octafile::load(Some(file_path)),
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
          cmd: echo "simple"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content);
    assert!(matches!(
      Octafile::load(Some(file_path)),
      Err(OctafileError::IoError(_))
    ));
  }

  #[test]
  fn test_working_directory() {
    let content = r#"
      version: 1
      tasks:
        test:
          dir: custom_dir
          cmd: echo "test"
    "#;
    let (_temp_dir, file_path) = create_temp_octafile(content);

    let octafile = Octafile::load(Some(file_path.clone())).unwrap();
    assert_eq!(
      octafile.dir,
      file_path.canonicalize().unwrap().parent().unwrap().to_path_buf()
    );
  }

  #[test]
  fn test_root_reference_consistency() {
    let root_content = r#"
      version: 1
      name: root
      includes:
        child:
          octafile: child/Octafile.yml
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    let child_content = r#"
      version: 1
      name: child
      includes:
        grandchild:
          octafile: grandchild/Octafile.yml
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    let grandchild_content = r#"
      version: 1
      name: grandchild
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    let temp_dir = TempDir::new().unwrap();
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

    let root = Octafile::load(Some(root_path)).unwrap();
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
      name: root
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    // Test with existing Octafile
    let temp_dir = TempDir::new().unwrap();
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content).unwrap();

    // Create nested directory structure
    let nested_dir = temp_dir.path().join("nested").join("deeply");
    fs::create_dir_all(&nested_dir).unwrap();

    // Change to nested directory and try to find Octafile
    let original_dir = env::current_dir().unwrap();
    env::set_current_dir(&nested_dir).unwrap();

    let found = Octafile::find_octafile().unwrap();
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
      let found = Octafile::find_octafile().unwrap();
      assert!(found.is_some());
      assert_eq!(
        found.unwrap().canonicalize().unwrap(),
        octafile_path.canonicalize().unwrap()
      );
    }

    // Test with no Octafile
    let empty_dir = TempDir::new().unwrap();
    env::set_current_dir(empty_dir.path()).unwrap();
    assert!(Octafile::find_octafile().unwrap().is_none());

    // Restore original directory
    env::set_current_dir(original_dir).unwrap();
  }

  #[test]
  fn test_hierarchy_and_relationships() {
    let root_content = r#"
      version: 1
      name: root
      includes:
        level1:
          octafile: level1/Octafile.yml
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    let level1_content = r#"
      version: 1
      name: level1
      includes:
        level2:
          octafile: level2/Octafile.yml
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    let level2_content = r#"
      version: 1
      name: level2
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    // Create directory structure
    let temp_dir = TempDir::new().unwrap();
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
    let root = Octafile::load(Some(root_path)).unwrap();

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
      name: root
      includes:
        first:
          octafile: first/Octafile.yml
        second:
          octafile: second/Octafile.yml
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    let child_content = r#"
      version: 1
      name: child
      tasks:
        simple:
          cmd: echo "simple"
    "#;

    // Setup directory structure
    let temp_dir = TempDir::new().unwrap();
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

    let root = Octafile::load(Some(root_path)).unwrap();

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
}
