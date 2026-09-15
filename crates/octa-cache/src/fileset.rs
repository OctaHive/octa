//! Ordered input-pattern expansion with hierarchical `.octaignore` rules.
//!
//! Positive patterns are reduced to a minimal set of scan roots, so overlapping
//! patterns share one traversal. Ordered include/exclude state is carried down
//! the tree: a matched directory selects its descendants until a later rule
//! changes the decision. Deeper ignore files override their parents, but an
//! ignored directory is pruned before its own ignore file is read, matching Git
//! semantics.

use std::{
  collections::{BTreeMap, HashMap},
  fs::{self, File},
  io::Read,
  path::{Component, Path, PathBuf},
  sync::Arc,
};

use glob::{MatchOptions, Pattern};
use ignore::{
  gitignore::{Gitignore, GitignoreBuilder},
  Match,
};
use octa_cache_protocol::{RelativePath, OCTA_WORKSPACE_STATE_DIRECTORY};
use octa_cache_protocol::{MAX_CACHE_LIST_ITEMS, MAX_CACHE_STRING_BYTES};
use tokio_util::sync::CancellationToken;

use crate::{
  error::{check_cancelled, io_error},
  platform::EntryKey,
  CacheError, CacheResult,
};

const OCTAIGNORE_FILE: &str = ".octaignore";
const MAX_OCTAIGNORE_BYTES: u64 = 1024 * 1024;

#[cfg(test)]
pub(crate) fn collect(
  patterns: &[String],
  root: &Path,
  max_entries: usize,
  cancel: &CancellationToken,
) -> CacheResult<Vec<PathBuf>> {
  collect_pattern_sets(std::iter::once(patterns), root, max_entries, cancel)
    .map(|entries| entries.into_iter().map(|entry| entry.path).collect())
}

/// One selected path together with metadata captured by the shared traversal.
///
/// Consumers must still validate the opened handle around content reads. The
/// retained metadata only avoids repeating the same path lookup immediately
/// after discovery.
pub(crate) struct DiscoveredEntry {
  pub(crate) path: PathBuf,
  pub(crate) metadata: fs::Metadata,
}

/// Expands independent ordered pattern sets and returns their filesystem union.
///
/// Keeping sets independent is important: an exclusion in one plugin's plan
/// must not remove an input required by the user or another plugin.
pub(crate) fn collect_sets(
  pattern_sets: &[Vec<String>],
  root: &Path,
  max_entries: usize,
  cancel: &CancellationToken,
) -> CacheResult<Vec<DiscoveredEntry>> {
  collect_pattern_sets(pattern_sets.iter().map(Vec::as_slice), root, max_entries, cancel)
}

fn collect_pattern_sets<'a>(
  pattern_sets: impl IntoIterator<Item = &'a [String]>,
  root: &Path,
  max_entries: usize,
  cancel: &CancellationToken,
) -> CacheResult<Vec<DiscoveredEntry>> {
  check_cancelled(cancel)?;
  if max_entries == 0 {
    return Err(CacheError::Configuration(
      "cache input max_entries must be nonzero".to_owned(),
    ));
  }
  let root = dunce::canonicalize(root).map_err(|error| io_error("canonicalize workspace", root, error))?;
  if !root.is_dir() {
    return Err(CacheError::Path {
      path: root,
      reason: "workspace must be a directory".to_owned(),
    });
  }

  let mut filter = IgnoreFilter::new(root.clone());
  let mut paths = BTreeMap::<PathBuf, fs::Metadata>::new();
  let mut visited = 0;
  let mut pattern_count = 0_usize;
  let mut patterns = Vec::new();
  let mut groups = Vec::new();
  for pattern_set in pattern_sets {
    pattern_count = pattern_count
      .checked_add(pattern_set.len())
      .ok_or_else(|| CacheError::Limit("cache input pattern count overflowed".to_owned()))?;
    if pattern_count > MAX_CACHE_LIST_ITEMS {
      return Err(CacheError::Configuration(format!(
        "cache input patterns are limited to {MAX_CACHE_LIST_ITEMS} items"
      )));
    }
    let start = patterns.len();
    patterns.extend(
      pattern_set
        .iter()
        .map(|value| InputPattern::parse(value, &root))
        .collect::<CacheResult<Vec<_>>>()?,
    );
    groups.push((start, patterns.len()));
  }

  // All producers share one traversal. Group ranges affect only the final
  // include/exclude decision, so overlapping user and plugin roots do not
  // multiply filesystem work.
  for scan_root in minimal_scan_roots(&patterns) {
    check_cancelled(cancel)?;
    let mut allow = |path: &Path, is_directory: bool| filter.is_ignored(path, is_directory).map(|ignored| !ignored);
    let mut walker = Walker {
      patterns: &patterns,
      groups: &groups,
      cancel,
      allow: &mut allow,
      paths: &mut paths,
      max_entries,
      visited: &mut visited,
    };
    let inherited = ancestor_matches(&patterns, &scan_root)?;
    walker.walk(&scan_root, &inherited)?;
  }
  Ok(
    paths
      .into_iter()
      .map(|(path, metadata)| DiscoveredEntry { path, metadata })
      .collect(),
  )
}

/// Validates input syntax and rejects output roots that a positive input scan
/// could traverse.
///
/// This intentionally uses the same parsed matcher and scan-root derivation as
/// snapshots. The conservative overlap check may require users of a workspace-
/// wide include to move generated outputs outside that include, which is safer
/// than publishing an action whose own outputs become future inputs.
#[cfg(test)]
pub(crate) fn validate_contract(patterns: &[String], root: &Path, outputs: &[RelativePath]) -> CacheResult<()> {
  validate_contract_sets(std::iter::once(patterns), root, outputs)
}

pub(crate) fn validate_contract_groups(
  pattern_sets: &[Vec<String>],
  root: &Path,
  outputs: &[RelativePath],
) -> CacheResult<()> {
  validate_contract_sets(pattern_sets.iter().map(Vec::as_slice), root, outputs)
}

fn validate_contract_sets<'a>(
  pattern_sets: impl IntoIterator<Item = &'a [String]>,
  root: &Path,
  outputs: &[RelativePath],
) -> CacheResult<()> {
  let root = dunce::canonicalize(root).map_err(|error| io_error("canonicalize workspace", root, error))?;
  let mut pattern_count = 0_usize;
  for pattern_set in pattern_sets {
    pattern_count = pattern_count
      .checked_add(pattern_set.len())
      .ok_or_else(|| CacheError::Limit("cache input pattern count overflowed".to_owned()))?;
    if pattern_count > MAX_CACHE_LIST_ITEMS {
      return Err(CacheError::Configuration(format!(
        "cache input patterns are limited to {MAX_CACHE_LIST_ITEMS} items"
      )));
    }
    let patterns = pattern_set
      .iter()
      .map(|value| InputPattern::parse(value, &root))
      .collect::<CacheResult<Vec<_>>>()?;
    for output in outputs {
      let output_path = root.join(output.as_str());
      if patterns.iter().any(|pattern| {
        !pattern.excluded
          && (output_path.starts_with(&pattern.scan_root) || pattern.scan_root.starts_with(&output_path))
      }) {
        return Err(CacheError::Configuration(format!(
          "cache output '{output}' overlaps the traversal of a positive input pattern"
        )));
      }
    }
  }
  Ok(())
}

fn ancestor_matches(patterns: &[InputPattern], path: &Path) -> CacheResult<Vec<bool>> {
  // A literal scan root may start below a directory selected or excluded by
  // an earlier rule. Seed the walker with that inherited ordered-rule state.
  let mut matches = vec![false; patterns.len()];
  for ancestor in path.ancestors().skip(1) {
    for (pattern, matched) in patterns.iter().zip(&mut matches) {
      *matched |= pattern.matches(ancestor, true)?;
    }
  }
  Ok(matches)
}

/// Parsed ordered rule together with the smallest traversal it can require.
struct InputPattern {
  /// Full absolute matcher used only during this discovery pass.
  matcher: Pattern,
  /// Deepest literal prefix from which this pattern can be discovered.
  scan_root: PathBuf,
  /// Remaining finite depth, or `None` when `**` permits arbitrary depth.
  max_depth: Option<usize>,
  /// Whether this ordered rule removes rather than selects matching entries.
  excluded: bool,
  /// Whether a trailing slash restricts a match to directories.
  require_directory: bool,
}

impl InputPattern {
  fn parse(value: &str, root: &Path) -> CacheResult<Self> {
    let (value, excluded) = if let Some(value) = value.strip_prefix("\\!") {
      (format!("!{value}"), false)
    } else if let Some(value) = value.strip_prefix('!') {
      (value.to_owned(), true)
    } else {
      (value.to_owned(), false)
    };
    if value.is_empty() || value.len() > MAX_CACHE_STRING_BYTES {
      return Err(CacheError::Configuration(
        "cache input pattern must be non-empty and bounded".to_owned(),
      ));
    }
    let path = Path::new(&value);
    let components = value.trim_end_matches('/').split('/');
    if path.is_absolute()
      || value.contains(['\\', ':'])
      || value.chars().any(char::is_control)
      || components
        .into_iter()
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
      return Err(CacheError::Configuration(format!(
        "cache input pattern '{value}' must be portable and workspace-relative"
      )));
    }
    let path = Path::new(&value);
    let pattern_text = format!("{}/{}", Pattern::escape(&normalize(root)?), value);
    let matcher = Pattern::new(pattern_text.trim_end_matches('/'))
      .map_err(|error| CacheError::Configuration(format!("invalid cache input pattern '{value}': {error}")))?;
    Ok(Self {
      matcher,
      scan_root: scan_root(path, root),
      max_depth: scan_depth(path),
      excluded,
      require_directory: value.ends_with('/'),
    })
  }

  fn matches(&self, path: &Path, is_directory: bool) -> CacheResult<bool> {
    Ok(
      self.matcher.matches_with(
        &normalize(path)?,
        MatchOptions {
          require_literal_separator: true,
          ..MatchOptions::new()
        },
      ) && (!self.require_directory || is_directory),
    )
  }

  fn may_match_below(&self, directory: &Path) -> bool {
    // Exclusions never justify scanning by themselves: there is nothing to
    // remove below a directory that no positive rule can select.
    if self.excluded {
      return false;
    }
    if self.scan_root.starts_with(directory) && self.scan_root != directory {
      return true;
    }
    let Ok(relative) = directory.strip_prefix(&self.scan_root) else {
      return false;
    };
    self
      .max_depth
      .is_none_or(|maximum| relative.components().count() < maximum)
  }
}

fn minimal_scan_roots(patterns: &[InputPattern]) -> Vec<PathBuf> {
  // Removing roots nested below another positive root is what makes
  // overlapping patterns share one filesystem traversal.
  let mut roots = patterns
    .iter()
    .filter(|pattern| !pattern.excluded)
    .map(|pattern| pattern.scan_root.clone())
    .collect::<Vec<_>>();
  roots.sort();
  roots.dedup();
  let mut minimal = Vec::<PathBuf>::new();
  for root in roots {
    if !minimal.iter().any(|ancestor| root.starts_with(ancestor)) {
      minimal.push(root);
    }
  }
  minimal
}

fn scan_root(value: &Path, root: &Path) -> PathBuf {
  let mut result = root.to_path_buf();
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

/// Depth-first traversal carrying ordered-rule matches inherited from parents.
struct Walker<'a, F> {
  patterns: &'a [InputPattern],
  /// Half-open pattern ranges that preserve each producer's ordered rules.
  groups: &'a [(usize, usize)],
  cancel: &'a CancellationToken,
  allow: &'a mut F,
  paths: &'a mut BTreeMap<PathBuf, fs::Metadata>,
  max_entries: usize,
  visited: &'a mut usize,
}

impl<F> Walker<'_, F>
where
  F: FnMut(&Path, bool) -> CacheResult<bool>,
{
  fn walk(&mut self, path: &Path, inherited_matches: &[bool]) -> CacheResult<()> {
    let mut pending = vec![(path.to_path_buf(), Arc::<[bool]>::from(inherited_matches))];
    while let Some((path, inherited_matches)) = pending.pop() {
      check_cancelled(self.cancel)?;
      if *self.visited >= self.max_entries {
        return Err(CacheError::Limit(format!(
          "input snapshot exceeds {} visited filesystem entries",
          self.max_entries
        )));
      }
      *self.visited += 1;
      let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
        Err(error) => return Err(io_error("inspect cache input", path, error)),
      };
      let is_directory = metadata.is_dir() && !metadata.file_type().is_symlink();
      if !(self.allow)(&path, is_directory)? {
        continue;
      }
      let matches = self
        .patterns
        .iter()
        .zip(inherited_matches.iter())
        .map(|(pattern, inherited)| {
          pattern
            .matches(&path, is_directory)
            .map(|current| current || *inherited)
        })
        .collect::<CacheResult<Arc<[bool]>>>()?;
      let selected = self.groups.iter().any(|(start, end)| {
        self.patterns[*start..*end]
          .iter()
          .zip(matches[*start..*end].iter())
          .filter(|(_, matched)| **matched)
          .map(|(pattern, _)| !pattern.excluded)
          .next_back()
          .unwrap_or(false)
      });
      // The last matching rule wins, while a directory match remains active
      // for descendants through the shared `matches` slice.
      if selected {
        self.paths.entry(path.clone()).or_insert_with(|| metadata.clone());
      }
      let selected_by_include = self
        .patterns
        .iter()
        .zip(matches.iter())
        .any(|(pattern, matched)| !pattern.excluded && *matched);
      if !is_directory || (!selected_by_include && !self.patterns.iter().any(|pattern| pattern.may_match_below(&path)))
      {
        continue;
      }

      // Bound the directory vector while it is populated. The pending entries
      // will each consume one unit from the same snapshot-wide budget.
      let remaining = self.max_entries - *self.visited;
      let mut entries = Vec::new();
      for entry in fs::read_dir(&path).map_err(|error| io_error("read cache input directory", &path, error))? {
        if entries.len() >= remaining {
          return Err(CacheError::Limit(format!(
            "input snapshot exceeds {} visited filesystem entries",
            self.max_entries
          )));
        }
        entries.push(
          entry
            .map_err(|error| io_error("read cache input directory entry", &path, error))?
            .path(),
        );
      }
      entries.sort_unstable();
      pending.extend(entries.into_iter().rev().map(|entry| (entry, matches.clone())));
    }
    Ok(())
  }
}

/// Lazily loaded hierarchy of `.octaignore` matchers for one workspace.
struct IgnoreFilter {
  root: PathBuf,
  matchers: HashMap<PathBuf, Option<Gitignore>>,
}

impl IgnoreFilter {
  fn new(root: PathBuf) -> Self {
    Self {
      root,
      matchers: HashMap::new(),
    }
  }

  fn is_ignored(&mut self, path: &Path, is_directory: bool) -> CacheResult<bool> {
    let relative = path.strip_prefix(&self.root).map_err(|_| CacheError::Path {
      path: path.to_path_buf(),
      reason: "input escaped the workspace".to_owned(),
    })?;
    if relative
      .components()
      .next()
      .is_some_and(|component| component.as_os_str() == OCTA_WORKSPACE_STATE_DIRECTORY)
    {
      return Ok(true);
    }
    let parent = relative.parent().unwrap_or_else(|| Path::new(""));
    let mut directory = self.root.clone();
    let mut active = Vec::new();
    self.load(&directory)?;
    if self.matchers[&directory].is_some() {
      active.push(directory.clone());
    }
    for component in parent.components() {
      directory.push(component);
      if self.matches(&active, &directory, true) {
        return Ok(true);
      }
      self.load(&directory)?;
      if self.matchers[&directory].is_some() {
        active.push(directory.clone());
      }
    }
    Ok(self.matches(&active, path, is_directory))
  }

  fn load(&mut self, directory: &Path) -> CacheResult<()> {
    if self.matchers.contains_key(directory) {
      return Ok(());
    }
    let path = directory.join(OCTAIGNORE_FILE);
    let Some(contents) = read_ignore_file(&path)? else {
      self.matchers.insert(directory.to_path_buf(), None);
      return Ok(());
    };
    let mut builder = GitignoreBuilder::new(directory);
    for (index, line) in contents.lines().enumerate() {
      let line = if index == 0 {
        line.trim_start_matches('\u{feff}')
      } else {
        line
      };
      builder.add_line(Some(path.clone()), line).map_err(|error| {
        CacheError::Configuration(format!(
          "failed to load '{}' at line {}: {error}",
          path.display(),
          index + 1
        ))
      })?;
    }
    let matcher = builder
      .build()
      .map_err(|error| CacheError::Configuration(format!("failed to load '{}': {error}", path.display())))?;
    self.matchers.insert(directory.to_path_buf(), Some(matcher));
    Ok(())
  }

  fn matches(&self, active: &[PathBuf], path: &Path, is_directory: bool) -> bool {
    let mut ignored = false;
    for directory in active {
      let matcher = self.matchers[directory]
        .as_ref()
        .expect("only directories with loaded ignore files become active");
      match matcher.matched(path, is_directory) {
        Match::Ignore(_) => ignored = true,
        Match::Whitelist(_) => ignored = false,
        Match::None => {},
      }
    }
    ignored
  }
}

/// Reads one bounded, stable, in-workspace ignore file without following links.
fn read_ignore_file(path: &Path) -> CacheResult<Option<String>> {
  let before = match fs::symlink_metadata(path) {
    Ok(metadata) => metadata,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
    Err(error) => return Err(io_error("inspect cache ignore file", path, error)),
  };
  if !before.is_file() || before.file_type().is_symlink() {
    return Err(CacheError::Path {
      path: path.to_path_buf(),
      reason: "cache ignore file must be a regular file, not a directory or symlink".to_owned(),
    });
  }
  if before.len() > MAX_OCTAIGNORE_BYTES {
    return Err(CacheError::Limit(format!(
      "cache ignore file '{}' exceeds {MAX_OCTAIGNORE_BYTES} bytes",
      path.display()
    )));
  }
  let before_key = EntryKey::new(path, &before)?;
  let mut file = File::open(path).map_err(|error| io_error("open cache ignore file", path, error))?;
  let opened = file
    .metadata()
    .map_err(|error| io_error("inspect open cache ignore file", path, error))?;
  if EntryKey::new(path, &opened)? != before_key {
    return Err(CacheError::UnstableFile {
      path: path.to_path_buf(),
    });
  }
  let mut contents = String::new();
  file
    .by_ref()
    .take(MAX_OCTAIGNORE_BYTES + 1)
    .read_to_string(&mut contents)
    .map_err(|error| io_error("read cache ignore file", path, error))?;
  let path_after = fs::symlink_metadata(path).map_err(|error| io_error("reinspect cache ignore file", path, error))?;
  let opened_after = file
    .metadata()
    .map_err(|error| io_error("reinspect open cache ignore file", path, error))?;
  if contents.len() as u64 > MAX_OCTAIGNORE_BYTES
    || !path_after.is_file()
    || path_after.file_type().is_symlink()
    || EntryKey::new(path, &path_after)? != before_key
    || EntryKey::new(path, &opened_after)? != before_key
  {
    return Err(CacheError::UnstableFile {
      path: path.to_path_buf(),
    });
  }
  Ok(Some(contents))
}

fn normalize(path: &Path) -> CacheResult<String> {
  let value = path.to_str().ok_or_else(|| CacheError::Path {
    path: path.to_path_buf(),
    reason: "portable cache paths must be UTF-8".to_owned(),
  })?;
  #[cfg(windows)]
  {
    Ok(value.replace('\\', "/"))
  }
  #[cfg(not(windows))]
  {
    if value.contains('\\') {
      return Err(CacheError::Path {
        path: path.to_path_buf(),
        reason: "portable cache paths must not contain backslashes".to_owned(),
      });
    }
    Ok(value.to_owned())
  }
}

#[cfg(test)]
mod tests {
  use tempfile::TempDir;

  use super::*;

  #[test]
  fn glob_walks_are_bounded_and_missing_patterns_are_empty() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("src/nested")).unwrap();
    fs::write(root.path().join("src/nested/main.rs"), "main").unwrap();

    let recursive = collect(
      &["src/**".to_owned()],
      root.path(),
      usize::MAX,
      &CancellationToken::new(),
    )
    .unwrap();
    let canonical_root = dunce::canonicalize(root.path()).unwrap();
    assert!(recursive.contains(&canonical_root.join("src/nested/main.rs")));
    assert!(collect(
      &["missing/*.rs".to_owned()],
      root.path(),
      usize::MAX,
      &CancellationToken::new()
    )
    .unwrap()
    .is_empty());
    assert_eq!(scan_depth(Path::new("src/**/*")), None);
    assert_eq!(scan_depth(Path::new("src/*.rs")), Some(1));
  }

  #[test]
  fn overlapping_patterns_collapse_to_minimal_scan_roots() {
    let root = TempDir::new().unwrap();
    let patterns = ["src/**", "src/lib/*.rs", "assets/*.svg", "!src/generated/**"]
      .iter()
      .map(|value| InputPattern::parse(value, root.path()).unwrap())
      .collect::<Vec<_>>();

    assert_eq!(
      minimal_scan_roots(&patterns),
      [root.path().join("assets"), root.path().join("src")]
    );
  }

  #[test]
  fn traversal_limit_is_shared_by_scan_roots_and_bounds_wide_directories() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("first"), "first").unwrap();
    fs::write(root.path().join("second"), "second").unwrap();
    let cancel = CancellationToken::new();

    assert!(matches!(
      collect(&["first".to_owned()], root.path(), 0, &cancel),
      Err(CacheError::Configuration(message)) if message.contains("max_entries")
    ));

    assert!(matches!(
      collect(&["first".to_owned(), "second".to_owned()], root.path(), 1, &cancel,),
      Err(CacheError::Limit(_))
    ));

    fs::create_dir(root.path().join("wide")).unwrap();
    fs::write(root.path().join("wide/one"), "one").unwrap();
    fs::write(root.path().join("wide/two"), "two").unwrap();
    assert!(matches!(
      collect(&["wide/**".to_owned()], root.path(), 2, &cancel),
      Err(CacheError::Limit(_))
    ));
  }

  #[test]
  fn overlapping_contract_groups_share_one_filesystem_traversal() {
    let root = TempDir::new().unwrap();
    fs::create_dir(root.path().join("src")).unwrap();
    fs::write(root.path().join("src/input"), "data").unwrap();
    let groups = vec![vec!["src/**".to_owned()], vec!["src/input".to_owned()]];

    let paths = collect_sets(&groups, root.path(), 2, &CancellationToken::new())
      .unwrap()
      .into_iter()
      .map(|entry| entry.path)
      .collect::<Vec<_>>();
    assert_eq!(paths, [dunce::canonicalize(root.path()).unwrap().join("src/input")]);
  }

  #[test]
  fn scan_pruning_distinguishes_exclusions_ancestors_and_unrelated_paths() {
    let root = TempDir::new().unwrap();
    let excluded = InputPattern::parse("!src/generated/**", root.path()).unwrap();
    assert!(!excluded.may_match_below(root.path()));

    let included = InputPattern::parse("src/deep/*.rs", root.path()).unwrap();
    assert!(included.may_match_below(root.path()));
    assert!(!included.may_match_below(&root.path().join("unrelated")));
  }

  #[test]
  fn ignored_parent_directories_are_pruned_before_nested_rules() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("generated/nested")).unwrap();
    fs::write(root.path().join("generated/nested/keep.rs"), "generated").unwrap();
    fs::write(root.path().join(".octaignore"), "generated/\n").unwrap();
    fs::write(root.path().join("generated/.octaignore"), "!nested/keep.rs\n").unwrap();

    assert!(collect(
      &["generated/**/*".to_owned()],
      root.path(),
      usize::MAX,
      &CancellationToken::new()
    )
    .unwrap()
    .is_empty());
  }

  #[test]
  fn ordered_ancestor_rules_apply_when_the_scan_starts_deeper() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join("src/deep")).unwrap();
    fs::write(root.path().join("src/deep/input"), "data").unwrap();
    let cancel = CancellationToken::new();

    assert!(collect(
      &["src/deep/input".to_owned(), "!src".to_owned()],
      root.path(),
      usize::MAX,
      &cancel,
    )
    .unwrap()
    .is_empty());
    assert_eq!(
      collect(
        &[
          "src/deep/input".to_owned(),
          "!src".to_owned(),
          "src/deep/input".to_owned(),
        ],
        root.path(),
        usize::MAX,
        &cancel,
      )
      .unwrap()
      .len(),
      1
    );
  }

  #[test]
  fn octa_workspace_state_is_never_an_input() {
    let root = TempDir::new().unwrap();
    fs::create_dir_all(root.path().join(".octa/cache")).unwrap();
    fs::write(root.path().join(".octa/cache/object"), "internal").unwrap();
    fs::write(root.path().join("input"), "public").unwrap();

    let paths = collect(&["**/*".to_owned()], root.path(), usize::MAX, &CancellationToken::new()).unwrap();
    assert_eq!(paths, vec![dunce::canonicalize(root.path()).unwrap().join("input")]);
  }

  #[test]
  fn validation_keeps_independent_input_contracts_separate() {
    let root = TempDir::new().unwrap();
    let output = RelativePath::new("generated").unwrap();
    let groups = vec![vec!["!generated/**".to_owned()], vec!["generated/schema".to_owned()]];

    let error = validate_contract_groups(&groups, root.path(), &[output]).unwrap_err();
    assert!(error.to_string().contains("overlaps"));
  }

  #[test]
  fn rejects_non_directory_roots_and_paths_outside_the_filter() {
    let root = TempDir::new().unwrap();
    let file = root.path().join("file");
    fs::write(&file, "file").unwrap();
    assert!(matches!(
      collect(&[], &file, usize::MAX, &CancellationToken::new()),
      Err(CacheError::Path { .. })
    ));

    let mut filter = IgnoreFilter::new(root.path().to_path_buf());
    assert!(matches!(
      filter.is_ignored(Path::new("/outside"), false),
      Err(CacheError::Path { .. })
    ));
  }

  #[test]
  fn input_patterns_have_one_bounded_cross_platform_shape() {
    let root = TempDir::new().unwrap();
    for invalid in ["", "C:/source/**", "a//b", "./source", "../source", "bad\nname"] {
      assert!(matches!(
        InputPattern::parse(invalid, root.path()),
        Err(CacheError::Configuration(_))
      ));
    }
    assert!(matches!(
      InputPattern::parse(&"x".repeat(MAX_CACHE_STRING_BYTES + 1), root.path()),
      Err(CacheError::Configuration(_))
    ));
    assert!(matches!(
      collect(
        &vec!["file".to_owned(); MAX_CACHE_LIST_ITEMS + 1],
        root.path(),
        usize::MAX,
        &CancellationToken::new()
      ),
      Err(CacheError::Configuration(_))
    ));
    assert!(matches!(
      validate_contract(&vec!["file".to_owned(); MAX_CACHE_LIST_ITEMS + 1], root.path(), &[]),
      Err(CacheError::Configuration(_))
    ));
  }

  #[cfg(unix)]
  #[test]
  fn discovery_rejects_a_literal_backslash_filename() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join(r"a\b"), "distinct Unix file").unwrap();

    assert!(matches!(
      collect(&["**".to_owned()], root.path(), usize::MAX, &CancellationToken::new()),
      Err(CacheError::Path { .. })
    ));
  }

  #[test]
  fn rejects_a_malformed_octaignore_file() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join(".octaignore"), "[z-a]\n").unwrap();
    assert!(matches!(
      collect(&["**/*".to_owned()], root.path(), usize::MAX, &CancellationToken::new()),
      Err(CacheError::Configuration(_))
    ));
  }

  #[test]
  fn rejects_an_oversized_or_non_regular_octaignore_file() {
    let root = TempDir::new().unwrap();
    fs::File::create(root.path().join(OCTAIGNORE_FILE))
      .unwrap()
      .set_len(MAX_OCTAIGNORE_BYTES + 1)
      .unwrap();
    assert!(matches!(
      collect(&["**".to_owned()], root.path(), usize::MAX, &CancellationToken::new()),
      Err(CacheError::Limit(_))
    ));

    fs::remove_file(root.path().join(OCTAIGNORE_FILE)).unwrap();
    fs::create_dir(root.path().join(OCTAIGNORE_FILE)).unwrap();
    assert!(matches!(
      collect(&["**".to_owned()], root.path(), usize::MAX, &CancellationToken::new()),
      Err(CacheError::Path { .. })
    ));
  }

  #[cfg(unix)]
  #[test]
  fn ignore_loading_never_follows_a_symbolic_link() {
    use std::os::unix::fs::symlink;

    let root = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("ignore");
    fs::write(&target, "secret/**\n").unwrap();
    symlink(&target, root.path().join(OCTAIGNORE_FILE)).unwrap();

    assert!(matches!(
      collect(&["**".to_owned()], root.path(), usize::MAX, &CancellationToken::new()),
      Err(CacheError::Path { .. })
    ));
    assert_eq!(fs::read_to_string(target).unwrap(), "secret/**\n");
  }

  #[cfg(unix)]
  #[test]
  fn rejects_non_utf8_portable_paths() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};

    assert!(normalize(Path::new(&OsString::from_vec(vec![0xff]))).is_err());
  }
}
