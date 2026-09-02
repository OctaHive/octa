//! Generated output discovery and ordered include/exclude checks.

use std::{
  collections::{BTreeSet, HashSet},
  fs,
  path::{Path, PathBuf},
  time::SystemTime,
};

use crate::{error::ExecutorResult, path_pattern::PathPattern};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub(crate) struct OutputState {
  pub(crate) missing: bool,
  oldest_modified: Option<SystemTime>,
}

impl OutputState {
  pub(crate) fn inspect(patterns: &[String], root: &Path, cancel_token: &CancellationToken) -> ExecutorResult<Self> {
    let mut paths = BTreeSet::<PathBuf>::new();
    let mut required_matches = Vec::new();

    for pattern in patterns {
      check_cancelled(cancel_token)?;
      let pattern = PathPattern::parse(pattern);
      let matches = pattern.expand(root, cancel_token, |_| Ok(true))?;
      if pattern.is_excluded() {
        paths.retain(|path| {
          !matches
            .iter()
            .any(|excluded| path == excluded || path.starts_with(excluded))
        });
      } else {
        let expanded = matches.into_iter().collect::<BTreeSet<_>>();
        paths.extend(expanded.iter().cloned());
        required_matches.push(expanded);
      }
    }

    let mut missing = required_matches.iter().any(|matches| matches.is_disjoint(&paths));
    let non_leaf_directories = non_leaf_directories(&paths);
    let mut oldest_modified = None;
    for path in paths {
      check_cancelled(cancel_token)?;
      match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && non_leaf_directories.contains(&path) => continue,
        Ok(metadata) => {
          let modified = metadata.modified()?;
          oldest_modified = Some(oldest_modified.map_or(modified, |oldest: SystemTime| oldest.min(modified)));
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => missing = true,
        Err(error) => return Err(error.into()),
      }
    }

    Ok(Self {
      missing,
      oldest_modified,
    })
  }

  pub(crate) fn is_older_than(&self, source_modified: Option<SystemTime>) -> bool {
    match (self.oldest_modified, source_modified) {
      (Some(output), Some(source)) => output < source,
      _ => false,
    }
  }
}

fn check_cancelled(cancel_token: &CancellationToken) -> ExecutorResult<()> {
  if cancel_token.is_cancelled() {
    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "output inspection cancelled").into());
  }
  Ok(())
}

fn non_leaf_directories(paths: &BTreeSet<PathBuf>) -> HashSet<PathBuf> {
  let mut result = HashSet::new();
  for path in paths {
    let mut ancestor = path.parent();
    while let Some(directory) = ancestor {
      if paths.contains(directory) {
        result.insert(directory.to_path_buf());
      }
      ancestor = directory.parent();
    }
  }
  result
}

#[cfg(test)]
mod tests {
  use super::*;
  use tempfile::TempDir;

  fn inspect(patterns: &[String], root: &Path) -> ExecutorResult<OutputState> {
    OutputState::inspect(patterns, root, &CancellationToken::new())
  }

  #[test]
  fn reports_missing_literal_and_glob_outputs() {
    let root = TempDir::new().unwrap();

    assert!(inspect(&["dist/app".to_owned()], root.path()).unwrap().missing);
    assert!(inspect(&["dist/*.js".to_owned()], root.path()).unwrap().missing);
  }

  #[test]
  fn requires_every_output_pattern_to_match() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join("dist")).unwrap();
    fs::write(root.path().join("dist/app.js"), "app").unwrap();

    let state = inspect(&["dist/*.js".to_owned(), "dist/*.css".to_owned()], root.path()).unwrap();

    assert!(state.missing);
  }

  #[test]
  fn compares_oldest_output_with_newest_source() {
    let root = TempDir::new().unwrap();
    let output = root.path().join("app");
    fs::write(&output, "app").unwrap();
    let output_modified = fs::metadata(output).unwrap().modified().unwrap();
    let state = inspect(&["app".to_owned()], root.path()).unwrap();

    assert!(!state.is_older_than(Some(output_modified)));
    assert!(!state.is_older_than(None));
  }

  #[test]
  fn directory_outputs_use_descendant_timestamps() {
    let root = TempDir::new().unwrap();
    let directory = root.path().join("dist");
    let output = directory.join("app");
    fs::create_dir(&directory).unwrap();
    fs::write(&output, "first").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(&output, "second").unwrap();
    let output_modified = fs::metadata(&output).unwrap().modified().unwrap();

    let state = inspect(&["dist".to_owned()], root.path()).unwrap();

    assert!(!state.missing);
    assert!(!state.is_older_than(Some(output_modified)));
  }

  #[test]
  fn excluding_a_directory_removes_its_descendants() {
    let root = TempDir::new().unwrap();
    let cache = root.path().join("dist/cache");
    fs::create_dir_all(&cache).unwrap();
    fs::write(cache.join("old.js"), "cache").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    fs::write(root.path().join("dist/app.js"), "app").unwrap();
    let app_modified = fs::metadata(root.path().join("dist/app.js"))
      .unwrap()
      .modified()
      .unwrap();

    let state = inspect(&["dist/**/*".to_owned(), "!dist/cache".to_owned()], root.path()).unwrap();

    assert!(!state.missing);
    assert!(!state.is_older_than(Some(app_modified)));
  }

  #[test]
  fn accepts_an_explicit_empty_output_list() {
    let root = TempDir::new().unwrap();
    let state = inspect(&[], root.path()).unwrap();

    assert!(!state.missing);
    assert!(!state.is_older_than(Some(SystemTime::now())));
  }

  #[test]
  fn excludes_outputs_and_allows_later_patterns_to_reinclude_them() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("app.js"), "app").unwrap();
    fs::write(root.path().join("app.js.map"), "map").unwrap();

    let excluded = inspect(&["*".to_owned(), "!*.map".to_owned()], root.path()).unwrap();
    assert!(!excluded.missing);

    let reincluded = inspect(
      &["*".to_owned(), "!*.map".to_owned(), "app.js.map".to_owned()],
      root.path(),
    )
    .unwrap();
    assert!(!reincluded.missing);
  }

  #[test]
  fn reports_missing_when_exclusions_remove_every_required_output() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("app.js.map"), "map").unwrap();

    let state = inspect(&["*.map".to_owned(), "!*.map".to_owned()], root.path()).unwrap();

    assert!(state.missing);
  }
}
