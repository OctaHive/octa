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
    let search_segments: Vec<&str> = path.split(':').collect();

    debug!("Searching in path: {}", path);

    self.search_recursive(octafile, &search_segments, 0, &mut results, String::new());

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

    for (key, task) in octafile.tasks.iter() {
      results.push(FindResult {
        name: self.join_path(path, &key),
        octafile: Arc::clone(&octafile),
        task: task.clone(),
      });
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
