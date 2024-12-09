use std::sync::Arc;

use octa_octafile::{Octafile, Task};
use tracing::debug;

/// Internal structure for task search results
#[derive(Debug)]
pub struct FindResult {
  pub name: String,
  pub octafile: Arc<Octafile>,
  pub task: Task,
}

pub struct OctaFinder {}

impl OctaFinder {
  pub fn new() -> Self {
    Self {}
  }

  /// Finds tasks by their path in the octafile hierarchy
  pub fn find_by_path(&self, octafile: Arc<Octafile>, path: &str) -> Vec<FindResult> {
    let mut results = Vec::new();
    let mut current = octafile;
    let mut current_path = path;

    // Search from root octafile
    if current_path.starts_with("::") {
      current = current.root().clone();
      current_path = current_path.strip_prefix("::").unwrap();
    }

    // Search from parent octafile
    if current_path.starts_with(":") {
      if let Some(parent) = current.parent() {
        current = parent.clone();
        current_path = current_path.strip_prefix(":").unwrap();
      } else {
        return results;
      }
    }

    let search_segments: Vec<&str> = current_path.split(':').collect();

    debug!("Searching in path: {}", path);

    self.search_recursive(current, &search_segments, 0, &mut results, String::new());

    debug!("Found {} results for path: {}", results.len(), path);
    results
  }

  /// Recursively searches for tasks matching the path pattern
  fn search_recursive(
    &self,
    octafile: Arc<Octafile>,
    search_segments: &[&str],
    depth: usize,
    results: &mut Vec<FindResult>,
    path: String,
  ) {
    if depth >= search_segments.len() {
      return;
    }

    match search_segments[depth] {
      "*" => self.handle_wildcard_search(octafile, search_segments, depth, results, &path),
      "**" => self.handle_recursive_search(octafile, search_segments, depth, results, &path),
      segment => self.handle_exact_search(octafile, search_segments, depth, results, &path, segment),
    }
  }

  /// Handles wildcard (*) search pattern
  fn handle_wildcard_search(
    &self,
    octafile: Arc<Octafile>,
    search_segments: &[&str],
    depth: usize,
    results: &mut Vec<FindResult>,
    path: &str,
  ) {
    for (name, octafile) in octafile.get_all_included().unwrap() {
      self.search_recursive(
        Arc::clone(&octafile),
        search_segments,
        depth + 1,
        results,
        self.join_path(path, &name),
      );
    }
  }

  /// Handles recursive (**) search pattern
  fn handle_recursive_search(
    &self,
    octafile: Arc<Octafile>,
    search_segments: &[&str],
    depth: usize,
    results: &mut Vec<FindResult>,
    path: &str,
  ) {
    let included = octafile.get_all_included().unwrap();
    if !included.is_empty() {
      for (name, octafile) in included {
        self.search_recursive(
          Arc::clone(&octafile),
          search_segments,
          depth,
          results,
          self.join_path(path, &name),
        );
      }
    }

    if search_segments.len() > 1 {
      let key = search_segments[1];
      if let Some(task) = octafile.tasks.get(key) {
        results.push(FindResult {
          name: self.join_path(path, &key),
          octafile: Arc::clone(&octafile),
          task: task.clone(),
        });
      }
    } else {
      for (key, task) in octafile.tasks.iter() {
        results.push(FindResult {
          name: self.join_path(path, &key),
          octafile: Arc::clone(&octafile),
          task: task.clone(),
        });
      }
    }
  }

  /// Handles exact segment match search
  fn handle_exact_search(
    &self,
    octafile: Arc<Octafile>,
    search_segments: &[&str],
    depth: usize,
    results: &mut Vec<FindResult>,
    path: &str,
    segment: &str,
  ) {
    if depth < search_segments.len() - 1 {
      // Check included octafiles
      if let Some(included) = octafile.get_all_included().unwrap().get(segment) {
        self.search_recursive(
          Arc::clone(included),
          search_segments,
          depth + 1,
          results,
          self.join_path(path, segment),
        );
      }
    } else {
      // Check for task match
      if let Some(task) = octafile.tasks.get(segment) {
        results.push(FindResult {
          name: self.join_path(path, segment),
          octafile: Arc::clone(&octafile),
          task: task.clone(),
        });
      }
    }
  }

  fn join_path(&self, current: &str, segment: &str) -> String {
    if current.is_empty() {
      segment.to_string()
    } else {
      format!("{}:{}", current, segment)
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use octa_octafile::AllowedRun;
  use std::path::PathBuf;
  use tempfile::TempDir;
  use test_log::test;
  use tracing_test::traced_test;

  fn create_test_task(name: &str) -> Task {
    Task {
      cmd: Some(format!("echo {}", name).into()),
      dir: None,
      vars: None,
      tpl: None,
      cmds: None,
      internal: Some(false),
      platforms: None,
      ignore_error: None,
      deps: None,
      run: Some(AllowedRun::Always),
    }
  }

  fn create_test_yaml(dir: &TempDir, name: &str, content: &str) -> PathBuf {
    let file_path = dir.path().join(name).join("Octafile.yml");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, content).unwrap();
    file_path
  }

  fn create_gen_test_yaml(name: Option<&str>, tasks: Vec<(&str, Task)>) -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let file_path = temp_dir.path().join("Octafile.yml");

    let content = {
      let mut content = String::from("version: 1\n");
      if let Some(name) = name {
        content.push_str(&format!("name: {}\n", name));
      }
      content.push_str("tasks:\n");
      for (task_name, task) in &tasks {
        content.push_str(&format!("  {}:\n", task_name));
        if let Some(ref cmd) = task.cmd {
          content.push_str(&format!("    cmd: {}\n", cmd));
        }
      }
      content
    };

    std::fs::write(&file_path, content).unwrap();
    (temp_dir, file_path)
  }

  #[traced_test]
  #[test]
  fn test_find_simple_task() {
    let finder = OctaFinder::new();
    let (_temp_dir, file_path) = create_gen_test_yaml(Some("root"), vec![("test", create_test_task("test"))]);

    let octafile = Octafile::load(Some(file_path)).unwrap();
    let results = finder.find_by_path(octafile, "test");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "test");
  }

  #[traced_test]
  #[test]
  fn test_find_nested_task() {
    let finder = OctaFinder::new();

    // Create child octafile
    let (child_dir, child_path) = create_gen_test_yaml(Some("child"), vec![("child_task", create_test_task("child"))]);

    // Create root octafile with include
    let root_content = format!(
      r#"
      version: 1
      name: root
      includes:
        child:
          octafile: {}
      tasks:
        root_task:
          cmd: echo root
      "#,
      child_path.display()
    );

    let temp_dir = tempfile::TempDir::new().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");
    std::fs::write(&root_path, root_content).unwrap();

    let root = Octafile::load(Some(root_path)).unwrap();
    let results = finder.find_by_path(root, "child:child_task");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "child:child_task");

    // Keep directories alive until test ends
    drop(temp_dir);
    drop(child_dir);
  }

  #[traced_test]
  #[test]
  fn test_wildcard_search() {
    let finder = OctaFinder::new();

    // Create child octafiles
    let (child1_dir, child1_path) = create_gen_test_yaml(Some("child1"), vec![("task", create_test_task("child1"))]);

    let (child2_dir, child2_path) = create_gen_test_yaml(Some("child2"), vec![("task", create_test_task("child2"))]);

    // Create root octafile with includes
    let root_content = format!(
      r#"
      version: 1
      name: root
      includes:
        child1:
          octafile: {}
        child2:
          octafile: {}
      tasks:
        root_task:
          cmd: echo root
      "#,
      child1_path.display(),
      child2_path.display()
    );

    let temp_dir = tempfile::TempDir::new().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");
    std::fs::write(&root_path, root_content).unwrap();

    let root = Octafile::load(Some(root_path)).unwrap();
    let results = finder.find_by_path(root, "*:task");

    assert_eq!(results.len(), 2);

    // Keep directories alive until test ends
    drop(temp_dir);
    drop(child1_dir);
    drop(child2_dir);
  }

  #[traced_test]
  #[test]
  fn test_recursive_search_tasks() {
    let finder = OctaFinder::new();
    let temp_dir = TempDir::new().unwrap();

    // Create a deep hierarchy:
    // root/
    //   ├── task1
    //   ├── level1/
    //   │   ├── task1
    //   │   └── level2/
    //   │       ├── task1
    //   │       └── level3/
    //   │           └── task1
    //   └── sibling/
    //       └── task1

    // Create level3 octafile
    let level3_content = r#"
      version: 1
      name: level3
      tasks:
        task1:
          cmd: echo level3_task
    "#;
    let level3_path = create_test_yaml(&temp_dir, "level1/level2/level3", level3_content);

    // Create level2 octafile
    let level2_content = format!(
      r#"
        version: 1
        name: level2
        includes:
          level3:
            octafile: {}
        tasks:
          task1:
            cmd: echo level2_task
        "#,
      level3_path.display()
    );
    let level2_path = create_test_yaml(&temp_dir, "level1/level2", &level2_content);

    // Create level1 octafile
    let level1_content = format!(
      r#"
      version: 1
      name: level1
      includes:
        level2:
          octafile: {}
      tasks:
        task1:
          cmd: echo level1_task
      "#,
      level2_path.display()
    );
    let level1_path = create_test_yaml(&temp_dir, "level1", &level1_content);

    // Create sibling octafile
    let sibling_content = r#"
      version: 1
      name: sibling
      tasks:
        task1:
          cmd: echo sibling_task
    "#;
    let sibling_path = create_test_yaml(&temp_dir, "sibling", sibling_content);

    // Create root octafile
    let root_content = format!(
      r#"
        version: 1
        name: root
        includes:
          level1:
            octafile: {}
          sibling:
            octafile: {}
        tasks:
          task1:
            cmd: echo root_task
        "#,
      level1_path.display(),
      sibling_path.display()
    );
    let root_path = create_test_yaml(&temp_dir, "", &root_content);

    let root = Octafile::load(Some(root_path)).unwrap();

    // Test recursive search for all task1
    let results = finder.find_by_path(root.clone(), "**:task1");
    assert_eq!(results.len(), 5); // Should find all task1 instances
    assert!(results.iter().any(|r| r.name == "task1")); // root
    assert!(results.iter().any(|r| r.name == "level1:task1"));
    assert!(results.iter().any(|r| r.name == "level1:level2:task1"));
    assert!(results.iter().any(|r| r.name == "level1:level2:level3:task1"));
    assert!(results.iter().any(|r| r.name == "sibling:task1"));

    // Test recursive search with specific depth
    // let results = finder.find_by_path(root.clone(), "level1:**:task1");
    // assert_eq!(results.len(), 3); // Should find level1 and deeper task1 instances
    // assert!(results.iter().any(|r| r.name == "level1:task1"));
    // assert!(results.iter().any(|r| r.name == "level1:level2:task1"));
    // assert!(results.iter().any(|r| r.name == "level1:level2:level3:task1"));

    // Test recursive search with non-existent task
    let results = finder.find_by_path(root.clone(), "**:nonexistent");
    assert_eq!(results.len(), 0);

    // Test recursive search with specific pattern
    // let results = finder.find_by_path(root.clone(), "**:level2:task1");
    // assert_eq!(results.len(), 1);
    // assert_eq!(results[0].name, "level1:level2:task1");
  }

  #[traced_test]
  #[test]
  fn test_recursive_search_with_optional_includes() {
    let finder = OctaFinder::new();
    let temp_dir = TempDir::new().unwrap();

    // Create a structure with optional includes
    let level1_content = r#"
      version: 1
      name: level1
      tasks:
        task1:
          cmd: echo level1_task
    "#;
    let level1_path = create_test_yaml(&temp_dir, "level1", level1_content);

    let root_content = format!(
      r#"
      version: 1
      name: root
      includes:
        level1:
          octafile: {}
        optional:
          octafile: nonexistent.yml
          optional: true
      tasks:
        task1:
          cmd: echo root_task
      "#,
      level1_path.display()
    );
    let root_path = create_test_yaml(&temp_dir, "", &root_content);

    let root = Octafile::load(Some(root_path)).unwrap();
    let results = finder.find_by_path(root, "**:task1");

    assert_eq!(results.len(), 2); // Should find both task1 instances
    assert!(results.iter().any(|r| r.name == "task1")); // root
    assert!(results.iter().any(|r| r.name == "level1:task1"));
  }

  #[traced_test]
  #[test]
  fn test_recursive_search_with_empty_includes() {
    let finder = OctaFinder::new();
    let temp_dir = TempDir::new().unwrap();

    let root_content = r#"
      version: 1
      name: root
      tasks:
        task1:
          cmd: echo root_task
    "#;
    let root_path = create_test_yaml(&temp_dir, "", root_content);

    let root = Octafile::load(Some(root_path)).unwrap();
    let results = finder.find_by_path(root, "**:task1");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "task1");
  }

  #[traced_test]
  #[test]
  fn test_recursive_search_all_tasks() {
    let finder = OctaFinder::new();
    let temp_dir = TempDir::new().unwrap();

    let level1_content = r#"
      version: 1
      name: level1
      tasks:
        task1:
          cmd: echo level1_task1
        task2:
          cmd: echo level1_task2
    "#;
    let level1_path = create_test_yaml(&temp_dir, "level1", level1_content);

    let root_content = format!(
      r#"
      version: 1
      name: root
      includes:
        level1:
          octafile: {}
      tasks:
        task1:
          cmd: echo root_task1
        task3:
          cmd: echo root_task3
      "#,
      level1_path.display()
    );
    let root_path = create_test_yaml(&temp_dir, "", &root_content);

    let root = Octafile::load(Some(root_path)).unwrap();

    // Test recursive search for all tasks
    let results = finder.find_by_path(root, "**");
    assert_eq!(results.len(), 4); // Should find all tasks in all files

    let task_names: Vec<String> = results.iter().map(|r| r.name.clone()).collect();
    assert!(task_names.contains(&"task1".to_string()));
    assert!(task_names.contains(&"task3".to_string()));
    assert!(task_names.contains(&"level1:task1".to_string()));
    assert!(task_names.contains(&"level1:task2".to_string()));
  }
}
