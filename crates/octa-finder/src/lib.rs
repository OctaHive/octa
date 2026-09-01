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

impl Default for OctaFinder {
  fn default() -> Self {
    Self::new()
  }
}

impl OctaFinder {
  pub fn new() -> Self {
    Self {}
  }

  /// Finds tasks by their path in the octafile hierarchy
  pub fn find_by_path(&self, octafile: Arc<Octafile>, path: &str) -> Vec<FindResult> {
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
        return Vec::new();
      }
    }

    debug!("Searching in path: {}", path);
    if !current_path.contains('*') {
      let results = self.find_exact(current, current_path).into_iter().collect::<Vec<_>>();
      debug!("Found {} results for path: {}", results.len(), path);
      return results;
    }

    let mut pattern = current.namespace_path();
    pattern.extend(current_path.split(':').map(str::to_owned));
    let mut results = Vec::new();
    self.collect_matching(current, &pattern, &mut results);
    results.sort_by(|left, right| left.name.cmp(&right.name));

    debug!("Found {} results for path: {}", results.len(), path);
    results
  }

  /// Finds tasks whose qualified name or description contains the query.
  pub fn search(&self, octafile: Arc<Octafile>, query: &str) -> Vec<FindResult> {
    let query = query.to_lowercase();

    self
      .find_by_path(octafile, "**")
      .into_iter()
      .filter(|result| {
        result.name.to_lowercase().contains(&query)
          || result
            .task
            .desc
            .as_ref()
            .is_some_and(|description| description.to_lowercase().contains(&query))
      })
      .collect()
  }

  fn collect_matching(&self, octafile: Arc<Octafile>, pattern: &[String], results: &mut Vec<FindResult>) {
    let namespace = octafile.namespace_path();
    for (key, task) in &octafile.tasks {
      let mut qualified = namespace.clone();
      qualified.push(key.clone());
      if matches_segments(pattern, &qualified) {
        results.push(FindResult {
          name: qualified.join(":"),
          octafile: Arc::clone(&octafile),
          task: task.clone(),
        });
      }
    }

    if let Ok(included) = octafile.get_all_included() {
      for included in included.into_values() {
        self.collect_matching(included, pattern, results);
      }
    }
  }

  fn find_exact(&self, octafile: Arc<Octafile>, path: &str) -> Option<FindResult> {
    let segments = path.split(':').collect::<Vec<_>>();
    let (task_name, namespace) = segments.split_last()?;
    if task_name.is_empty() || namespace.iter().any(|segment| segment.is_empty()) {
      return None;
    }

    let octafile = Self::find_descendant(octafile, namespace)?;
    let task = octafile.tasks.get(*task_name)?.clone();
    let mut qualified = octafile.namespace_path();
    qualified.push((*task_name).to_owned());
    Some(FindResult {
      name: qualified.join(":"),
      octafile,
      task,
    })
  }

  fn find_descendant(octafile: Arc<Octafile>, namespace: &[&str]) -> Option<Arc<Octafile>> {
    if namespace.is_empty() {
      return Some(octafile);
    }

    for consumed in (1..=namespace.len()).rev() {
      // A discovered project is a direct include whose key may cover several logical segments.
      let name = namespace[..consumed].join(":");
      let Some(included) = octafile.get_included(&name).ok()? else {
        continue;
      };
      if let Some(found) = Self::find_descendant(included, &namespace[consumed..]) {
        return Some(found);
      }
    }

    None
  }
}

fn matches_segments(pattern: &[String], candidate: &[String]) -> bool {
  match (pattern.split_first(), candidate.split_first()) {
    (None, None) => true,
    (None, Some(_)) => false,
    (Some((head, tail)), _) if head == "**" => {
      matches_segments(tail, candidate)
        || candidate
          .split_first()
          .is_some_and(|(_, candidate_tail)| matches_segments(pattern, candidate_tail))
    },
    (Some(_), None) => false,
    (Some((head, tail)), Some((candidate_head, candidate_tail))) => {
      (head == "*" || head == candidate_head) && matches_segments(tail, candidate_tail)
    },
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use octa_octafile::{PluginCommand, SyntheticInclude};
  use serde_yml::Value;
  use std::path::PathBuf;
  use tempfile::TempDir;
  use test_log::test;
  use tracing_test::traced_test;

  fn create_test_task(name: &str) -> Task {
    Task {
      plugin: Some(PluginCommand {
        key: "shell".to_owned(),
        value: Value::String(format!("echo {}", name)),
      }),
      ..Task::default()
    }
  }

  fn create_test_yaml(dir: &TempDir, name: &str, content: &str) -> PathBuf {
    let file_path = dir.path().join(name).join("Octafile.yml");
    std::fs::create_dir_all(file_path.parent().unwrap()).unwrap();
    std::fs::write(&file_path, content).unwrap();
    file_path
  }

  fn create_gen_test_yaml(tasks: Vec<(&str, Task)>) -> (tempfile::TempDir, PathBuf) {
    let temp_dir = tempfile::TempDir::new().unwrap();
    let file_path = temp_dir.path().join("Octafile.yml");

    let content = {
      let mut content = String::from("version: 1\n");

      content.push_str("tasks:\n");
      for (task_name, task) in &tasks {
        content.push_str(&format!("  {}:\n", task_name));
        if let Some(plugin) = &task.plugin {
          let value = serde_yml::to_string(&plugin.value).unwrap();
          content.push_str(&format!("    {}: {}\n", plugin.key, value));
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
    let (_temp_dir, file_path) = create_gen_test_yaml(vec![("test", create_test_task("test"))]);

    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();
    let results = finder.find_by_path(octafile, "test");

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "test");
  }

  #[test]
  fn test_exact_search_rejects_invalid_and_missing_paths() {
    let finder = OctaFinder::new();
    let (_temp_dir, file_path) = create_gen_test_yaml(vec![("test", create_test_task("test"))]);
    let octafile = Octafile::load(Some(file_path), false, vec!["shell".to_string()], "shell").unwrap();

    assert!(finder.find_by_path(Arc::clone(&octafile), "").is_empty());
    assert!(finder.find_by_path(Arc::clone(&octafile), "child:").is_empty());
    assert!(finder.find_by_path(octafile, "missing:test").is_empty());
  }

  #[traced_test]
  #[test]
  fn test_find_nested_task() {
    let finder = OctaFinder::new();

    // Create child octafile
    let (child_dir, child_path) = create_gen_test_yaml(vec![("child_task", create_test_task("child"))]);

    // Create root octafile with include
    let root_content = format!(
      r#"
      version: 1

      includes:
        child:
          octafile: {}
      tasks:
        root_task:
          shell: echo root
      "#,
      child_path.display()
    );

    let temp_dir = tempfile::TempDir::new().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");
    std::fs::write(&root_path, root_content).unwrap();

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();
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
    let (child1_dir, child1_path) = create_gen_test_yaml(vec![("task", create_test_task("child1"))]);

    let (child2_dir, child2_path) = create_gen_test_yaml(vec![("task", create_test_task("child2"))]);

    // Create root octafile with includes
    let root_content = format!(
      r#"
      version: 1

      includes:
        child1:
          octafile: {}
        child2:
          octafile: {}
      tasks:
        root_task:
          shell: echo root
      "#,
      child1_path.display(),
      child2_path.display()
    );

    let temp_dir = tempfile::TempDir::new().unwrap();
    let root_path = temp_dir.path().join("Octafile.yml");
    std::fs::write(&root_path, root_content).unwrap();

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();
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

      tasks:
        task1:
          shell: echo level3_task
    "#;
    let level3_path = create_test_yaml(&temp_dir, "level1/level2/level3", level3_content);

    // Create level2 octafile
    let level2_content = format!(
      r#"
        version: 1

        includes:
          level3:
            octafile: {}
        tasks:
          task1:
            shell: echo level2_task
        "#,
      level3_path.display()
    );
    let level2_path = create_test_yaml(&temp_dir, "level1/level2", &level2_content);

    // Create level1 octafile
    let level1_content = format!(
      r#"
      version: 1

      includes:
        level2:
          octafile: {}
      tasks:
        task1:
          shell: echo level1_task
      "#,
      level2_path.display()
    );
    let level1_path = create_test_yaml(&temp_dir, "level1", &level1_content);

    // Create sibling octafile
    let sibling_content = r#"
      version: 1

      tasks:
        task1:
          shell: echo sibling_task
    "#;
    let sibling_path = create_test_yaml(&temp_dir, "sibling", sibling_content);

    // Create root octafile
    let root_content = format!(
      r#"
        version: 1

        includes:
          level1:
            octafile: {}
          sibling:
            octafile: {}
        tasks:
          task1:
            shell: echo root_task
        "#,
      level1_path.display(),
      sibling_path.display()
    );
    let root_path = create_test_yaml(&temp_dir, "", &root_content);

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();

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

      tasks:
        task1:
          shell: echo level1_task
    "#;
    let level1_path = create_test_yaml(&temp_dir, "level1", level1_content);

    let root_content = format!(
      r#"
      version: 1

      includes:
        level1:
          octafile: {}
        optional:
          octafile: nonexistent.yml
          optional: true
      tasks:
        task1:
          shell: echo root_task
      "#,
      level1_path.display()
    );
    let root_path = create_test_yaml(&temp_dir, "", &root_content);

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();
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

      tasks:
        task1:
          shell: echo root_task
    "#;
    let root_path = create_test_yaml(&temp_dir, "", root_content);

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();
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

      tasks:
        task1:
          shell: echo level1_task1
        task2:
          shell: echo level1_task2
    "#;
    let level1_path = create_test_yaml(&temp_dir, "level1", level1_content);

    let root_content = format!(
      r#"
      version: 1

      includes:
        level1:
          octafile: {}

      tasks:
        task1:
          shell: echo root_task1
        task3:
          shell: echo root_task3
      "#,
      level1_path.display()
    );
    let root_path = create_test_yaml(&temp_dir, "", &root_content);

    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();

    // Test recursive search for all tasks
    let results = finder.find_by_path(root, "**");
    assert_eq!(results.len(), 4); // Should find all tasks in all files

    let task_names: Vec<String> = results.iter().map(|r| r.name.clone()).collect();
    assert!(task_names.contains(&"task1".to_string()));
    assert!(task_names.contains(&"task3".to_string()));
    assert!(task_names.contains(&"level1:task1".to_string()));
    assert!(task_names.contains(&"level1:task2".to_string()));
  }

  #[test]
  fn test_search_tasks_by_qualified_name_and_description() {
    let temp_dir = TempDir::new().unwrap();
    let child_content = r#"
      version: 1
      tasks:
        deploy:
          desc: Publish service
          shell: echo deploy
    "#;
    let child_path = create_test_yaml(&temp_dir, "backend", child_content);
    let root_content = format!(
      r#"
      version: 1
      includes:
        backend:
          octafile: {}
      tasks:
        build:
          desc: Compile application
          shell: echo build
      "#,
      child_path.display()
    );
    let root_path = create_test_yaml(&temp_dir, "", &root_content);
    let root = Octafile::load(Some(root_path), false, vec!["shell".to_string()], "shell").unwrap();
    let finder = OctaFinder::new();

    let by_name = finder.search(Arc::clone(&root), "BACKEND:DEP");
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0].name, "backend:deploy");

    let by_description = finder.search(Arc::clone(&root), "compile");
    assert_eq!(by_description.len(), 1);
    assert_eq!(by_description[0].name, "build");

    assert!(finder.search(root, "missing").is_empty());
  }

  #[test]
  fn test_discovered_projects_use_the_same_colon_wildcards_as_includes() {
    let temp_dir = TempDir::new().unwrap();
    let root_path = create_test_yaml(&temp_dir, "", "version: 1\ntasks: {}\n");
    let api_path = create_test_yaml(&temp_dir, "packages/api", "version: 1\ntasks:\n  build: echo api\n");
    let web_path = create_test_yaml(&temp_dir, "packages/web", "version: 1\ntasks:\n  build: echo web\n");
    let root = Octafile::load_with_schemas_vars_and_includes_from(
      Some(root_path),
      false,
      None,
      std::collections::HashMap::from([("shell".to_owned(), None)]),
      "shell",
      &[],
      &[
        SyntheticInclude {
          namespace: vec!["packages".to_owned(), "api".to_owned()],
          path: api_path,
        },
        SyntheticInclude {
          namespace: vec!["packages".to_owned(), "web".to_owned()],
          path: web_path,
        },
      ],
    )
    .unwrap();
    let finder = OctaFinder::new();

    let exact = finder.find_by_path(Arc::clone(&root), "packages:api:build");
    assert_eq!(
      exact.iter().map(|result| result.name.as_str()).collect::<Vec<_>>(),
      ["packages:api:build"]
    );
    let wildcard = finder.find_by_path(Arc::clone(&root), "packages:*:build");
    assert_eq!(
      wildcard.iter().map(|result| result.name.as_str()).collect::<Vec<_>>(),
      ["packages:api:build", "packages:web:build"]
    );
    let recursive = finder.find_by_path(root, "packages:**");
    assert_eq!(recursive.len(), 2);
  }
}
