use std::{collections::HashSet, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
  error::{OctafileError, OctafileResult},
  parser::{self, location_error},
};

fn default_max_depth() -> usize {
  5
}

/// Controls automatic Octafile discovery below a monorepo root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MonorepoConfig {
  /// Relative directory patterns whose matching directories may contain Octafiles.
  pub roots: Vec<String>,
  /// Directory names or relative glob patterns pruned from discovery.
  #[serde(default)]
  pub exclude: Vec<String>,
  /// Maximum traversal depth for roots containing a recursive `**` segment.
  #[serde(default = "default_max_depth")]
  pub max_depth: usize,
}

impl MonorepoConfig {
  pub fn validate(&self) -> Result<(), String> {
    if self.roots.is_empty() {
      return Err("monorepo roots must not be empty".to_owned());
    }
    if self.max_depth == 0 {
      return Err("monorepo max_depth must be greater than zero".to_owned());
    }

    Ok(())
  }
}

/// Reads only the monorepo section using the same parser as full Octafile loading.
pub fn read_monorepo_config(path: &Path) -> OctafileResult<Option<MonorepoConfig>> {
  let path_str = path.display().to_string();
  let map = parser::parse_file(path)?
    .into_mapping()
    .ok_or_else(|| OctafileError::ParseError(path_str.clone(), "Expected mapping".to_owned()))?;
  let mut fields = HashSet::new();
  let mut config = None;

  for (key, value) in map {
    let marker = key.marker();
    let key = key.as_str().ok_or_else(|| {
      OctafileError::ParseError(
        path_str.clone(),
        location_error(marker, "Octafile keys must be strings"),
      )
    })?;
    if !fields.insert(key.to_owned()) {
      return Err(OctafileError::ParseError(
        path_str,
        location_error(marker, &format!("duplicated key '{key}'")),
      ));
    }
    if key == "monorepo" {
      let value = value
        .into_value()
        .map_err(|error| OctafileError::ParseError(path_str.clone(), error))?;
      let parsed: MonorepoConfig =
        serde_yml::from_value(value).map_err(|error| OctafileError::ParseError(path_str.clone(), error.to_string()))?;
      parsed
        .validate()
        .map_err(|error| OctafileError::ParseError(path_str.clone(), error))?;
      config = Some(parsed);
    }
  }

  Ok(config)
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;

  use super::*;

  fn config_file(content: &str) -> (TempDir, std::path::PathBuf) {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("Octafile.yml");
    fs::write(&path, content).unwrap();
    (directory, path)
  }

  #[test]
  fn probes_monorepo_without_deserializing_tasks() {
    let (_directory, path) =
      config_file("version: 1\nmonorepo:\n  roots: [packages/*]\ntasks:\n  render: !custom hello\n");

    let config = read_monorepo_config(&path).unwrap().unwrap();
    assert_eq!(config.roots, ["packages/*"]);
  }

  #[test]
  fn probe_rejects_invalid_document_shapes_and_duplicate_fields() {
    for content in [
      "- version\n- 1\n",
      "1: value\n",
      "version: 1\nversion: 1\n",
      "monorepo:\n  roots: []\n",
    ] {
      let (_directory, path) = config_file(content);
      assert!(read_monorepo_config(&path).is_err(), "accepted {content}");
    }
  }
}
