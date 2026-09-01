use std::{
  collections::{BTreeMap, HashSet},
  path::{Component, Path, PathBuf},
};

use globset::{GlobBuilder, GlobMatcher};
use ignore::WalkBuilder;
use octa_octafile::{MonorepoConfig, Octafile};

use crate::{model::DiscoveryResult, MonorepoError, MonorepoProject};

const ALWAYS_EXCLUDED_DIRECTORIES: [&str; 2] = [".git", ".octa"];

#[derive(Clone)]
struct Excludes {
  names: HashSet<String>,
  patterns: Vec<GlobMatcher>,
}

pub(crate) fn discover(
  root: &Path,
  root_octafile: &Path,
  config: &MonorepoConfig,
) -> Result<DiscoveryResult, MonorepoError> {
  let excludes = Excludes::new(&config.exclude)?;
  let mut directories = BTreeMap::new();
  let mut projects = BTreeMap::new();
  record_nearest_directory(root, root, &mut directories);

  for pattern in &config.roots {
    let normalized = normalize_pattern(pattern)?;
    let matcher = compile_pattern(&normalized)?;
    let (prefix, depth) = pattern_walk(&normalized, config.max_depth);
    let search_root = root.join(prefix);
    if !search_root.is_dir() {
      record_nearest_directory(root, &search_root, &mut directories);
      continue;
    }

    let filter_root = root.to_path_buf();
    let filter_excludes = excludes.clone();
    let mut builder = WalkBuilder::new(&search_root);
    builder
      .max_depth(Some(depth))
      .follow_links(false)
      .hidden(false)
      .git_ignore(false)
      .git_global(false)
      .git_exclude(false)
      .filter_entry(move |entry| {
        entry
          .path()
          .strip_prefix(&filter_root)
          .map(|path| !filter_excludes.matches(path))
          .unwrap_or(true)
      });

    for entry in builder.build() {
      let entry = entry?;
      if !entry.file_type().is_some_and(|file_type| file_type.is_dir()) {
        continue;
      }
      let relative = entry
        .path()
        .strip_prefix(root)
        .expect("walked paths stay below the monorepo root");
      if excludes.matches(relative) {
        continue;
      }
      record_directory(root, entry.path(), &mut directories);

      if !matcher.is_match(path_text(relative)) {
        continue;
      }
      let Some(octafile) = Octafile::find_in_directory(entry.path()) else {
        continue;
      };
      let octafile = octafile.canonicalize()?;
      if octafile == root_octafile {
        continue;
      }

      let namespace = namespace(relative)?;
      projects.insert(octafile.clone(), MonorepoProject { namespace, octafile });
    }
  }

  let mut projects = projects.into_values().collect::<Vec<_>>();
  projects.sort_by(|left, right| left.namespace.cmp(&right.namespace));
  Ok(DiscoveryResult {
    directories: directories.into_values().collect(),
    projects,
  })
}

fn normalize_pattern(pattern: &str) -> Result<String, MonorepoError> {
  let pattern = pattern.trim().trim_start_matches("./").replace('\\', "/");
  let path = Path::new(&pattern);
  if pattern.is_empty() || path.is_absolute() || path.components().any(|component| component == Component::ParentDir) {
    return Err(MonorepoError::InvalidPattern {
      pattern,
      message: "patterns must be relative and stay within the monorepo root".to_owned(),
    });
  }
  Ok(pattern)
}

fn compile_pattern(pattern: &str) -> Result<GlobMatcher, MonorepoError> {
  GlobBuilder::new(pattern)
    .literal_separator(true)
    .backslash_escape(false)
    .build()
    .map(|glob| glob.compile_matcher())
    .map_err(|error| MonorepoError::InvalidPattern {
      pattern: pattern.to_owned(),
      message: error.to_string(),
    })
}

fn pattern_walk(pattern: &str, max_depth: usize) -> (PathBuf, usize) {
  // Start at the non-glob prefix to avoid walking unrelated parts of the repository.
  let segments = pattern.split('/').collect::<Vec<_>>();
  let wildcard = segments
    .iter()
    .position(|segment| segment.contains(['*', '?', '[', '{']))
    .unwrap_or(segments.len());
  let prefix = segments[..wildcard].iter().collect::<PathBuf>();
  let remaining = &segments[wildcard..];
  let depth = if remaining.iter().any(|segment| segment.contains("**")) {
    max_depth
  } else {
    remaining.len()
  };
  (prefix, depth)
}

impl Excludes {
  fn new(excludes: &[String]) -> Result<Self, MonorepoError> {
    let mut names: HashSet<String> = ALWAYS_EXCLUDED_DIRECTORIES
      .iter()
      .map(|name| (*name).to_owned())
      .collect();
    let mut patterns = Vec::new();

    for exclude in excludes {
      let normalized = normalize_pattern(exclude)?;
      if !normalized.contains('/') {
        names.insert(normalized);
      } else {
        patterns.push(compile_pattern(&normalized)?);
      }
    }

    Ok(Self { names, patterns })
  }

  fn matches(&self, path: &Path) -> bool {
    path
      .components()
      .filter_map(|component| match component {
        Component::Normal(name) => Some(name.to_string_lossy()),
        _ => None,
      })
      .any(|name| self.names.contains(name.as_ref()))
      || self.patterns.iter().any(|pattern| pattern.is_match(path_text(path)))
  }
}

fn namespace(path: &Path) -> Result<Vec<String>, MonorepoError> {
  let mut namespace = Vec::new();
  for component in path.components() {
    let Component::Normal(component) = component else {
      continue;
    };
    let component = component.to_string_lossy().into_owned();
    if component.contains(':') {
      return Err(MonorepoError::InvalidConfiguration(format!(
        "project path component '{component}' cannot contain ':'"
      )));
    }
    namespace.push(component);
  }
  Ok(namespace)
}

fn record_nearest_directory(root: &Path, path: &Path, directories: &mut BTreeMap<PathBuf, PathBuf>) {
  if let Some(directory) = path.ancestors().find(|path| path.is_dir() && path.starts_with(root)) {
    record_directory(root, directory, directories);
  }
}

fn record_directory(root: &Path, directory: &Path, directories: &mut BTreeMap<PathBuf, PathBuf>) {
  let relative = directory.strip_prefix(root).unwrap_or(Path::new("")).to_path_buf();
  directories.insert(relative, directory.to_path_buf());
}

fn path_text(path: &Path) -> String {
  path.to_string_lossy().replace('\\', "/")
}
