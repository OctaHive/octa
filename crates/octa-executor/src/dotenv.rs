use std::{
  collections::HashMap,
  env,
  path::{Path, PathBuf},
};

use tera::{Context, Tera};

use crate::{
  error::{ExecutorError, ExecutorResult},
  vars::Vars,
};

/// Loads configured dotenv entries, preserving their declared priority order.
pub(crate) fn load(
  patterns: Option<&[String]>,
  start_dir: &Path,
  vars: &Vars,
  environment: &HashMap<String, String>,
) -> ExecutorResult<HashMap<String, String>> {
  let Some(patterns) = patterns else {
    return Ok(HashMap::new());
  };

  let mut values = HashMap::new();

  // Read lower-priority files first so earlier entries can overwrite their values.
  for pattern in patterns.iter().rev() {
    let rendered = render(pattern, vars, environment)?;
    let path = resolve(&rendered, start_dir);
    let Some(path) = path else {
      continue;
    };

    let entries = dotenvy::from_path_iter(&path).map_err(|source| ExecutorError::DotenvError {
      path: path.display().to_string(),
      source,
    })?;

    for entry in entries {
      let (key, value) = entry.map_err(|source| ExecutorError::DotenvError {
        path: path.display().to_string(),
        source,
      })?;
      values.insert(key, value);
    }
  }

  Ok(values)
}

/// Expands a dotenv entry with Octa variables and the environment collected so far.
fn render(pattern: &str, vars: &Vars, environment: &HashMap<String, String>) -> ExecutorResult<String> {
  let mut context = Context::from_serialize(environment)
    .map_err(|error| ExecutorError::ValueExpandError(pattern.to_string(), error.to_string()))?;
  context.extend(
    Context::from_serialize(vars.to_merged_hashmap())
      .map_err(|error| ExecutorError::ValueExpandError(pattern.to_string(), error.to_string()))?,
  );

  let rendered = Tera::default()
    .render_str(pattern, &context)
    .map_err(|error| ExecutorError::ValueExpandError(pattern.to_string(), error.to_string()))?;

  let get_env = |name: &str| environment.get(name).cloned().or_else(|| env::var(name).ok());

  Ok(shellexpand::env_with_context_no_errors(&rendered, get_env).into_owned())
}

/// Resolves paths directly and searches bare file names through the ancestor directories.
fn resolve(value: &str, start_dir: &Path) -> Option<PathBuf> {
  let path = Path::new(value);

  if is_explicit_path(path) {
    let path = if path.is_absolute() {
      path.to_path_buf()
    } else {
      start_dir.join(path)
    };
    return Some(path);
  }

  let mut directory = if start_dir.is_absolute() {
    start_dir.to_path_buf()
  } else {
    env::current_dir().ok()?.join(start_dir)
  };

  loop {
    let candidate = directory.join(path);
    if candidate.is_file() {
      return Some(candidate);
    }
    if !directory.pop() {
      return None;
    }
  }
}

fn is_explicit_path(path: &Path) -> bool {
  path.is_absolute() || path.parent().is_some_and(|parent| !parent.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  #[test]
  fn searches_file_names_upward() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("a").join("b");
    fs::create_dir_all(&nested).unwrap();
    fs::write(root.path().join(".env.test"), "VALUE=found\n").unwrap();

    let values = load(
      Some(&[".env.{{ PROFILE }}".to_string()]),
      &nested,
      &Vars::with_value(serde_json::json!({ "PROFILE": "test" })),
      &HashMap::new(),
    )
    .unwrap();

    assert_eq!(values["VALUE"], "found");
  }

  #[test]
  fn treats_values_with_directories_as_explicit_paths() {
    let root = TempDir::new().unwrap();
    let nested = root.path().join("nested");
    fs::create_dir_all(root.path().join("config")).unwrap();
    fs::create_dir_all(&nested).unwrap();
    fs::write(root.path().join("config").join("settings.env"), "VALUE=parent\n").unwrap();

    let error = load(
      Some(&["config/settings.env".to_string()]),
      &nested,
      &Vars::new(),
      &HashMap::new(),
    )
    .unwrap_err();

    assert!(matches!(error, ExecutorError::DotenvError { .. }));
  }

  #[test]
  fn gives_the_first_file_highest_priority() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("first.env"), "VALUE=first\n").unwrap();
    fs::write(root.path().join("second.env"), "VALUE=second\n").unwrap();

    let values = load(
      Some(&["./first.env".to_string(), "./second.env".to_string()]),
      root.path(),
      &Vars::new(),
      &HashMap::new(),
    )
    .unwrap();

    assert_eq!(values["VALUE"], "first");
  }

  #[test]
  fn ignores_missing_searched_file_names() {
    let root = TempDir::new().unwrap();

    let values = load(
      Some(&["missing.env".to_string()]),
      root.path(),
      &Vars::new(),
      &HashMap::new(),
    )
    .unwrap();

    assert!(values.is_empty());
  }

  #[test]
  fn loads_an_absolute_path() {
    let root = TempDir::new().unwrap();
    let dotenv = root.path().join("absolute.env");
    fs::write(&dotenv, "VALUE=absolute\n").unwrap();

    let values = load(
      Some(&[dotenv.to_string_lossy().into_owned()]),
      Path::new("unused"),
      &Vars::new(),
      &HashMap::new(),
    )
    .unwrap();

    assert_eq!(values["VALUE"], "absolute");
  }

  #[test]
  fn reports_invalid_dotenv_entries() {
    let root = TempDir::new().unwrap();
    fs::write(root.path().join("invalid.env"), "INVALID LINE\n").unwrap();

    let error = load(
      Some(&["./invalid.env".to_string()]),
      root.path(),
      &Vars::new(),
      &HashMap::new(),
    )
    .unwrap_err();

    assert!(matches!(error, ExecutorError::DotenvError { .. }));
  }
}
