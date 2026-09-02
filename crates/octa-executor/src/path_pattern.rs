//! Filesystem pattern expansion shared by source and output discovery.

use std::{
  collections::BTreeSet,
  fs,
  path::{Component, Path, PathBuf},
};

use glob::{MatchOptions, Pattern};
use tokio_util::sync::CancellationToken;

use crate::error::{ExecutorError, ExecutorResult};

pub(crate) struct PathPattern {
  value: String,
  excluded: bool,
}

impl PathPattern {
  pub(crate) fn parse(value: &str) -> Self {
    // A leading backslash escapes the exclusion marker without becoming part of the path.
    if let Some(value) = value.strip_prefix("\\!") {
      Self {
        value: format!("!{value}"),
        excluded: false,
      }
    } else if let Some(value) = value.strip_prefix('!') {
      Self {
        value: value.to_owned(),
        excluded: true,
      }
    } else {
      Self {
        value: value.to_owned(),
        excluded: false,
      }
    }
  }

  pub(crate) fn is_excluded(&self) -> bool {
    self.excluded
  }

  /// Expands a pattern and includes descendants of every matched directory.
  ///
  /// `allow` lets source discovery prune ignored directories before they are
  /// read, while output discovery can accept every path.
  pub(crate) fn expand<F>(
    &self,
    root: &Path,
    cancel_token: &CancellationToken,
    mut allow: F,
  ) -> ExecutorResult<Vec<PathBuf>>
  where
    F: FnMut(&Path) -> ExecutorResult<bool>,
  {
    check_cancelled(cancel_token)?;
    if self.value.is_empty() {
      return Ok(Vec::new());
    }
    let value = Path::new(&self.value);
    let scan_root = scan_root(value, root);
    let max_depth = scan_depth(value);
    let pattern = compile_pattern(value, root)?;
    let require_directory = self.value.ends_with('/') || self.value.ends_with('\\');
    let mut walker = Walker {
      pattern: &pattern,
      require_directory,
      max_depth,
      cancel_token,
      allow: &mut allow,
      paths: BTreeSet::new(),
    };

    match fs::symlink_metadata(&scan_root) {
      Ok(_) => walker.walk(&scan_root, false, 0)?,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
      Err(error) => return Err(error.into()),
    }

    Ok(walker.paths.into_iter().collect())
  }
}

fn scan_root(value: &Path, root: &Path) -> PathBuf {
  let mut result = if value.is_absolute() {
    PathBuf::new()
  } else {
    root.to_path_buf()
  };

  for component in value.components() {
    if component_has_metacharacters(component) {
      break;
    }
    result.push(component.as_os_str());
  }

  result
}

fn component_has_metacharacters(component: Component<'_>) -> bool {
  component
    .as_os_str()
    .to_string_lossy()
    .chars()
    .any(|character| matches!(character, '*' | '?' | '['))
}

fn scan_depth(value: &Path) -> Option<usize> {
  let mut pattern_started = false;
  let mut depth = 0;

  for component in value.components() {
    if pattern_started || component_has_metacharacters(component) {
      pattern_started = true;
      if component.as_os_str() == "**" {
        return None;
      }
      depth += 1;
    }
  }

  Some(depth)
}

fn compile_pattern(value: &Path, root: &Path) -> ExecutorResult<Pattern> {
  let pattern = if value.is_absolute() {
    normalize(value)
  } else {
    let root = Pattern::escape(&normalize(root));
    let value = normalize(value);
    format!("{root}/{value}")
  };
  let pattern = pattern.trim_end_matches('/');
  Pattern::new(pattern).map_err(ExecutorError::ExtendSourceError)
}

fn normalize(path: &Path) -> String {
  path.to_string_lossy().replace('\\', "/")
}

struct Walker<'a, F> {
  pattern: &'a Pattern,
  require_directory: bool,
  max_depth: Option<usize>,
  cancel_token: &'a CancellationToken,
  allow: &'a mut F,
  paths: BTreeSet<PathBuf>,
}

impl<F> Walker<'_, F>
where
  F: FnMut(&Path) -> ExecutorResult<bool>,
{
  fn walk(&mut self, path: &Path, ancestor_matched: bool, depth: usize) -> ExecutorResult<()> {
    check_cancelled(self.cancel_token)?;
    if !(self.allow)(path)? {
      return Ok(());
    }

    let metadata = match fs::symlink_metadata(path) {
      Ok(metadata) => metadata,
      Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
      Err(error) => return Err(error.into()),
    };
    let is_directory = metadata.is_dir() && !metadata.file_type().is_symlink();
    let matched = ancestor_matched
      || (self.pattern.matches_with(
        &normalize(path),
        MatchOptions {
          require_literal_separator: true,
          ..MatchOptions::new()
        },
      ) && (!self.require_directory || is_directory));

    if matched {
      self.paths.insert(path.to_path_buf());
    }
    if !is_directory {
      return Ok(());
    }
    if !matched && self.max_depth.is_some_and(|max_depth| depth >= max_depth) {
      return Ok(());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
      check_cancelled(self.cancel_token)?;
      entries.push(entry?.path());
    }
    entries.sort_unstable();
    for entry in entries {
      self.walk(&entry, matched, depth + 1)?;
    }
    Ok(())
  }
}

fn check_cancelled(cancel_token: &CancellationToken) -> ExecutorResult<()> {
  if cancel_token.is_cancelled() {
    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "path expansion cancelled").into());
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  fn expand(pattern: &str, root: &Path) -> ExecutorResult<Vec<PathBuf>> {
    PathPattern::parse(pattern).expand(root, &CancellationToken::new(), |_| Ok(true))
  }

  #[test]
  fn parses_exclusions_and_escaped_bang_paths() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("!important.txt"), "important").unwrap();

    let excluded = PathPattern::parse("!*.tmp");
    assert!(excluded.is_excluded());

    let literal = PathPattern::parse(r"\!important.txt");
    assert!(!literal.is_excluded());
    assert_eq!(
      literal
        .expand(root.path(), &CancellationToken::new(), |_| Ok(true))
        .unwrap(),
      vec![root.path().join("!important.txt")]
    );
  }

  #[test]
  fn empty_pattern_does_not_select_the_project_root() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("source.txt"), "source").unwrap();

    assert!(expand("", root.path()).unwrap().is_empty());
  }

  #[test]
  fn expands_matched_directories_without_a_second_glob_walk() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("src/nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("main.rs"), "source").unwrap();

    assert_eq!(
      expand("src/*", root.path()).unwrap(),
      vec![nested.clone(), nested.join("main.rs")]
    );
  }

  #[test]
  fn shallow_pattern_does_not_read_below_its_matching_depth() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("nested");
    let nested_source = nested.join("nested.rs");
    let direct_source = root.path().join("direct.rs");
    fs::create_dir(&nested).unwrap();
    fs::write(&nested_source, "nested").unwrap();
    fs::write(&direct_source, "direct").unwrap();
    let mut visited = Vec::new();

    let paths = PathPattern::parse("*.rs")
      .expand(root.path(), &CancellationToken::new(), |path| {
        visited.push(path.to_path_buf());
        Ok(true)
      })
      .unwrap();

    assert_eq!(paths, vec![direct_source]);
    assert!(!visited.contains(&nested_source));
  }

  #[test]
  fn callback_prunes_a_directory_before_it_is_read() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("src/generated");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("bindings.rs"), "generated").unwrap();
    let generated = nested.clone();

    let paths = PathPattern::parse("src/**/*")
      .expand(root.path(), &CancellationToken::new(), |path| Ok(path != generated))
      .unwrap();

    assert!(!paths.iter().any(|path| path.starts_with(&nested)));
  }

  #[test]
  fn cancellable_expansion_stops_before_returning_matches() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("source.txt"), "source").unwrap();
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let result = PathPattern::parse("*.txt").expand(root.path(), &cancel_token, |_| Ok(true));

    assert!(matches!(
      result,
      Err(ExecutorError::IoError(error)) if error.kind() == std::io::ErrorKind::Interrupted
    ));
  }
}
