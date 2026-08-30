//! Source glob expansion with hierarchical `.octaignore` filtering.
//!
//! Ignore files are applied from the root Octafile directory down to the
//! source file. Rules from deeper directories take precedence over rules from
//! their parents, matching Git's ignore-file behavior.

use std::{
  collections::HashMap,
  path::{Path, PathBuf},
};

use glob::glob;
use ignore::{
  gitignore::{Gitignore, GitignoreBuilder},
  Match,
};

use crate::error::{ExecutorError, ExecutorResult};

const OCTAIGNORE_FILE: &str = ".octaignore";

/// Expands source globs and removes paths excluded by applicable `.octaignore` files.
pub(crate) fn collect(sources: &[String], root: &Path) -> ExecutorResult<Vec<PathBuf>> {
  let current_dir = dunce::canonicalize(std::env::current_dir()?)?;
  let root = if root.is_absolute() {
    root.to_path_buf()
  } else {
    current_dir.join(root)
  };
  let root = dunce::canonicalize(root)?;
  let mut filter = SourceFilter::new(root, current_dir);
  let mut paths = Vec::new();

  for source in sources {
    for entry in glob(source)? {
      let path = entry.map_err(ExecutorError::GlobError)?;
      if !filter.is_ignored(&path)? {
        paths.push(path);
      }
    }
  }

  Ok(paths)
}

struct SourceFilter {
  root: PathBuf,
  current_dir: PathBuf,
  // `None` records directories without `.octaignore` to avoid repeated filesystem checks.
  matchers: HashMap<PathBuf, Option<Gitignore>>,
}

impl SourceFilter {
  fn new(root: PathBuf, current_dir: PathBuf) -> Self {
    Self {
      root,
      current_dir,
      matchers: HashMap::new(),
    }
  }

  fn is_ignored(&mut self, path: &Path) -> ExecutorResult<bool> {
    let absolute_path = if path.is_absolute() {
      path.to_path_buf()
    } else {
      self.current_dir.join(path)
    };
    let absolute_path = dunce::canonicalize(absolute_path)?;

    // Ignore files outside the root Octafile must not affect external sources.
    let Ok(relative_path) = absolute_path.strip_prefix(&self.root) else {
      return Ok(false);
    };

    let parent = relative_path.parent().unwrap_or_else(|| Path::new(""));
    let mut directory = self.root.clone();
    let mut active_matchers = Vec::new();

    self.load_matcher(&directory)?;
    if self.matchers[&directory].is_some() {
      active_matchers.push(directory.clone());
    }

    for component in parent.components() {
      directory.push(component);

      // Git does not read ignore files inside an already ignored directory,
      // so descendants cannot re-include files until the directory itself is included.
      if self.matches(&active_matchers, &directory, true) {
        return Ok(true);
      }

      self.load_matcher(&directory)?;
      if self.matchers[&directory].is_some() {
        active_matchers.push(directory.clone());
      }
    }

    Ok(self.matches(&active_matchers, &absolute_path, path.is_dir()))
  }

  fn load_matcher(&mut self, directory: &Path) -> ExecutorResult<()> {
    if self.matchers.contains_key(directory) {
      return Ok(());
    }

    let ignore_path = directory.join(OCTAIGNORE_FILE);
    if !ignore_path.is_file() {
      self.matchers.insert(directory.to_path_buf(), None);
      return Ok(());
    }

    let mut builder = GitignoreBuilder::new(directory);
    if let Some(error) = builder.add(ignore_path) {
      return Err(error.into());
    }

    self.matchers.insert(directory.to_path_buf(), Some(builder.build()?));
    Ok(())
  }

  fn matches(&self, active_matchers: &[PathBuf], path: &Path, is_dir: bool) -> bool {
    let mut ignored = false;

    // Matchers are ordered from the project root to the nearest directory;
    // every explicit match replaces the decision made by its parents.
    for directory in active_matchers {
      let Some(matcher) = self.matchers[directory].as_ref() else {
        continue;
      };

      match matcher.matched(path, is_dir) {
        Match::Ignore(_) => ignored = true,
        Match::Whitelist(_) => ignored = false,
        Match::None => {},
      }
    }

    ignored
  }
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  fn glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
  }

  #[test]
  fn ignores_files_and_directory_contents() {
    let root = TempDir::new().unwrap();
    let src = root.path().join("src");
    let generated = src.join("generated");
    fs::create_dir_all(&generated).unwrap();
    fs::write(src.join("main.rs"), "fn main() {}").unwrap();
    fs::write(src.join("debug.log"), "debug").unwrap();
    fs::write(generated.join("bindings.rs"), "generated").unwrap();
    fs::write(root.path().join(OCTAIGNORE_FILE), "*.log\nsrc/generated/\n").unwrap();

    let sources = vec![format!("{}/**/*", glob_path(&src))];
    let paths = collect(&sources, root.path()).unwrap();

    assert_eq!(paths, vec![src.join("main.rs")]);
  }

  #[test]
  fn supports_negated_patterns() {
    let root = TempDir::new().unwrap();
    let logs = root.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    fs::write(logs.join("debug.log"), "debug").unwrap();
    fs::write(logs.join("important.log"), "important").unwrap();
    fs::write(root.path().join(OCTAIGNORE_FILE), "*.log\n!important.log\n").unwrap();

    let sources = vec![format!("{}/*.log", glob_path(&logs))];
    let paths = collect(&sources, root.path()).unwrap();

    assert_eq!(paths, vec![logs.join("important.log")]);
  }

  #[test]
  fn nested_octaignore_only_applies_to_its_descendants() {
    let root = TempDir::new().unwrap();
    let src = root.path().join("src");
    let tests = root.path().join("tests");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&tests).unwrap();
    fs::write(src.join("cache.tmp"), "src cache").unwrap();
    fs::write(tests.join("cache.tmp"), "test cache").unwrap();
    fs::write(src.join(OCTAIGNORE_FILE), "*.tmp\n").unwrap();

    let sources = vec![format!("{}/**/*.tmp", glob_path(root.path()))];
    let paths = collect(&sources, root.path()).unwrap();

    assert_eq!(paths, vec![tests.join("cache.tmp")]);
  }

  #[test]
  fn nested_octaignore_overrides_parent_rules() {
    let root = TempDir::new().unwrap();
    let src = root.path().join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(root.path().join("root.log"), "root").unwrap();
    fs::write(src.join("debug.log"), "debug").unwrap();
    fs::write(src.join("important.log"), "important").unwrap();
    fs::write(root.path().join(OCTAIGNORE_FILE), "*.log\n").unwrap();
    fs::write(src.join(OCTAIGNORE_FILE), "!important.log\n").unwrap();

    let sources = vec![
      format!("{}/*.log", glob_path(root.path())),
      format!("{}/*.log", glob_path(&src)),
    ];
    let paths = collect(&sources, root.path()).unwrap();

    assert_eq!(paths, vec![src.join("important.log")]);
  }

  #[test]
  fn cannot_reinclude_file_from_ignored_parent_directory() {
    let root = TempDir::new().unwrap();
    let generated = root.path().join("generated");
    fs::create_dir_all(&generated).unwrap();
    fs::write(generated.join("keep.rs"), "generated").unwrap();
    fs::write(root.path().join(OCTAIGNORE_FILE), "generated/\n").unwrap();
    fs::write(generated.join(OCTAIGNORE_FILE), "!keep.rs\n").unwrap();

    let sources = vec![format!("{}/*.rs", glob_path(&generated))];
    let paths = collect(&sources, root.path()).unwrap();

    assert!(paths.is_empty());
  }

  #[test]
  fn does_not_ignore_sources_outside_project_root() {
    let root = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let external_file = external.path().join("external.txt");
    fs::write(&external_file, "external").unwrap();
    fs::write(root.path().join(OCTAIGNORE_FILE), "*.txt\n").unwrap();

    let paths = collect(&[glob_path(&external_file)], root.path()).unwrap();

    assert_eq!(paths, vec![external_file]);
  }

  #[test]
  fn includes_all_sources_when_octaignore_is_missing() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, "source").unwrap();

    let paths = collect(&[glob_path(&source)], root.path()).unwrap();

    assert_eq!(paths, vec![source]);
  }

  #[test]
  fn supports_canonicalized_project_root() {
    let root = TempDir::new().unwrap();
    let src = root.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("tracked.txt"), "tracked").unwrap();
    fs::write(src.join("ignored.txt"), "ignored").unwrap();
    fs::write(src.join(OCTAIGNORE_FILE), "ignored.txt\n").unwrap();

    let canonical_root = fs::canonicalize(root.path()).unwrap();
    let sources = vec![format!("{}/*.txt", glob_path(&src))];
    let paths = collect(&sources, &canonical_root).unwrap();

    assert_eq!(paths, vec![src.join("tracked.txt")]);
  }

  #[test]
  fn returns_no_sources_when_glob_has_no_matches() {
    let root = TempDir::new().unwrap();
    let missing = root.path().join("missing").join("*.txt");

    let paths = collect(&[glob_path(&missing)], root.path()).unwrap();

    assert!(paths.is_empty());
  }
}
