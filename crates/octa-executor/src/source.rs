//! Source glob expansion with hierarchical `.octaignore` filtering.
//!
//! Ignore files are applied from the root Octafile directory down to the
//! source file. Rules from deeper directories take precedence over rules from
//! their parents, matching Git's ignore-file behavior. Inline include and
//! exclude patterns are evaluated in declaration order before this filter.

use std::{
  collections::{BTreeSet, HashMap},
  fs,
  path::{Path, PathBuf},
  time::SystemTime,
};

use ignore::{
  gitignore::{Gitignore, GitignoreBuilder},
  Match,
};
use tokio_util::sync::CancellationToken;

use crate::{error::ExecutorResult, path_pattern::PathPattern};

const OCTAIGNORE_FILE: &str = ".octaignore";

/// Expands source globs and removes paths excluded by applicable `.octaignore` files.
pub(crate) fn collect(
  sources: &[String],
  root: &Path,
  cancel_token: &CancellationToken,
) -> ExecutorResult<Vec<PathBuf>> {
  check_cancelled(cancel_token)?;
  let current_dir = dunce::canonicalize(std::env::current_dir()?)?;
  let root = if root.is_absolute() {
    root.to_path_buf()
  } else {
    current_dir.join(root)
  };
  let root = dunce::canonicalize(root)?;
  let mut filter = SourceFilter::new(root.clone(), current_dir);
  let mut paths = BTreeSet::<PathBuf>::new();

  for source in sources {
    check_cancelled(cancel_token)?;
    let pattern = PathPattern::parse(source);
    let matches = pattern.expand(&root, cancel_token, |path| {
      filter.is_ignored(path).map(|ignored| !ignored)
    })?;
    if pattern.is_excluded() {
      for path in matches {
        paths.retain(|candidate| candidate != &path && !candidate.starts_with(&path));
      }
    } else {
      paths.extend(matches);
    }
  }

  Ok(paths.into_iter().collect())
}

fn check_cancelled(cancel_token: &CancellationToken) -> ExecutorResult<()> {
  if cancel_token.is_cancelled() {
    return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "source collection cancelled").into());
  }
  Ok(())
}

pub(crate) fn newest_modified(
  paths: &[PathBuf],
  cancel_token: &CancellationToken,
) -> ExecutorResult<Option<SystemTime>> {
  check_cancelled(cancel_token)?;
  let mut newest = None;
  for path in paths {
    check_cancelled(cancel_token)?;
    let modified = fs::symlink_metadata(path)?.modified()?;
    newest = Some(newest.map_or(modified, |current: SystemTime| current.max(modified)));
  }
  Ok(newest)
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
    // Resolve aliases in the parent path while preserving the final component when it is a
    // symlink. Ignore patterns apply to the source name, not to its target.
    let absolute_path = match (absolute_path.parent(), absolute_path.file_name()) {
      (Some(parent), Some(name)) => dunce::canonicalize(parent)?.join(name),
      _ => dunce::canonicalize(absolute_path)?,
    };

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

    let is_dir = fs::symlink_metadata(path)?.file_type().is_dir();
    Ok(self.matches(&active_matchers, &absolute_path, is_dir))
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

  fn collect_sources(sources: &[String], root: &Path) -> ExecutorResult<Vec<PathBuf>> {
    collect(sources, root, &CancellationToken::new())
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
    let paths = collect_sources(&sources, root.path()).unwrap();

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
    let paths = collect_sources(&sources, root.path()).unwrap();

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
    let paths = collect_sources(&sources, root.path()).unwrap();

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
    let paths = collect_sources(&sources, root.path()).unwrap();

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
    let paths = collect_sources(&sources, root.path()).unwrap();

    assert!(paths.is_empty());
  }

  #[test]
  fn does_not_ignore_sources_outside_project_root() {
    let root = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let external_file = external.path().join("external.txt");
    fs::write(&external_file, "external").unwrap();
    fs::write(root.path().join(OCTAIGNORE_FILE), "*.txt\n").unwrap();

    let paths = collect_sources(&[glob_path(&external_file)], root.path()).unwrap();

    assert_eq!(paths, vec![external_file]);
  }

  #[cfg(unix)]
  #[test]
  fn octaignore_matches_the_symlink_name_instead_of_its_target() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let target = external.path().join("target.txt");
    let link = root.path().join("ignored.txt");
    fs::write(&target, "external").unwrap();
    symlink(target, &link).unwrap();
    fs::write(root.path().join(OCTAIGNORE_FILE), "ignored.txt\n").unwrap();

    let paths = collect_sources(&[glob_path(&link)], root.path()).unwrap();

    assert!(paths.is_empty());
  }

  #[test]
  fn includes_all_sources_when_octaignore_is_missing() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("source.txt");
    fs::write(&source, "source").unwrap();

    let paths = collect_sources(&[glob_path(&source)], root.path()).unwrap();

    assert_eq!(paths, vec![source]);
  }

  #[test]
  fn resolves_relative_patterns_from_project_root() {
    let root = TempDir::new().unwrap();
    let source_dir = root.path().join("src");
    fs::create_dir(&source_dir).unwrap();
    fs::write(source_dir.join("main.rs"), "source").unwrap();

    let paths = collect_sources(&["src/*.rs".to_owned()], root.path()).unwrap();

    assert_eq!(paths, vec![dunce::canonicalize(source_dir.join("main.rs")).unwrap()]);
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
    let paths = collect_sources(&sources, &canonical_root).unwrap();

    assert_eq!(paths, vec![src.join("tracked.txt")]);
  }

  #[test]
  fn returns_no_sources_when_glob_has_no_matches() {
    let root = TempDir::new().unwrap();
    let missing = root.path().join("missing").join("*.txt");

    let paths = collect_sources(&[glob_path(&missing)], root.path()).unwrap();

    assert!(paths.is_empty());
  }

  #[test]
  fn applies_inline_exclusions_in_declaration_order() {
    let root = TempDir::new().unwrap();
    let src = root.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("main.rs"), "main").unwrap();
    fs::write(src.join("generated.rs"), "generated").unwrap();

    let paths = collect_sources(
      &[
        "src/*.rs".to_owned(),
        "!src/generated.rs".to_owned(),
        "src/generated.rs".to_owned(),
      ],
      root.path(),
    )
    .unwrap();

    assert_eq!(
      paths,
      vec![
        dunce::canonicalize(src.join("generated.rs")).unwrap(),
        dunce::canonicalize(src.join("main.rs")).unwrap(),
      ]
    );
  }

  #[test]
  fn reincluding_a_directory_restores_its_descendants() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("src").join("nested");
    fs::create_dir_all(&nested).unwrap();
    let source = nested.join("main.rs");
    fs::write(&source, "main").unwrap();

    let paths = collect_sources(
      &["src".to_owned(), "!src/nested/*".to_owned(), "src/nested".to_owned()],
      root.path(),
    )
    .unwrap();

    assert!(paths.contains(&dunce::canonicalize(source).unwrap()));
  }

  #[test]
  fn supports_literal_paths_starting_with_bang() {
    let root = TempDir::new().unwrap();
    let source = root.path().join("!important.txt");
    fs::write(&source, "important").unwrap();

    let paths = collect_sources(&[r"\!important.txt".to_owned()], root.path()).unwrap();

    assert_eq!(paths, vec![dunce::canonicalize(source).unwrap()]);
  }

  #[test]
  fn directory_sources_include_nested_contents() {
    let root = TempDir::new().unwrap();
    let source_dir = root.path().join("src");
    let nested = source_dir.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let source = nested.join("main.rs");
    fs::write(&source, "fn main() {}").unwrap();

    let paths = collect_sources(&[glob_path(&source_dir)], root.path()).unwrap();

    assert_eq!(paths, vec![source_dir, nested, source]);
  }

  #[test]
  fn excluding_a_directory_removes_its_descendants() {
    let root = TempDir::new().unwrap();
    let source_dir = root.path().join("src");
    let generated = source_dir.join("generated");
    fs::create_dir_all(&generated).unwrap();
    let main = source_dir.join("main.rs");
    fs::write(&main, "fn main() {}").unwrap();
    fs::write(generated.join("bindings.rs"), "generated").unwrap();

    let paths = collect_sources(
      &[glob_path(&source_dir), format!("!{}", glob_path(&generated))],
      root.path(),
    )
    .unwrap();

    assert_eq!(paths, vec![source_dir, main]);
  }

  #[test]
  fn newest_timestamp_check_honors_cancellation() {
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let result = newest_modified(&[], &cancel_token);

    assert!(matches!(
      result,
      Err(crate::error::ExecutorError::IoError(error)) if error.kind() == std::io::ErrorKind::Interrupted
    ));
  }
}
