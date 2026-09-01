use std::{path::Path, time::UNIX_EPOCH};

use octa_octafile::MonorepoConfig;
use serde::{Deserialize, Serialize};

use crate::{model::DiscoveryResult, MonorepoError, MonorepoProject};

const CACHE_TREE: &str = "monorepo_discovery_v1";
const CACHE_VERSION: u8 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct DirectoryStamp {
  path: String,
  modified_seconds: u64,
  modified_nanos: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct CachedProject {
  namespace: Vec<String>,
  octafile: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct CacheEntry {
  version: u8,
  config: MonorepoConfig,
  directories: Vec<DirectoryStamp>,
  projects: Vec<CachedProject>,
}

pub(crate) fn load(
  cache: &sled::Db,
  root_octafile: &Path,
  root: &Path,
  config: &MonorepoConfig,
) -> Result<Option<Vec<MonorepoProject>>, MonorepoError> {
  let tree = cache.open_tree(CACHE_TREE)?;
  let key = root_octafile.as_os_str().as_encoded_bytes();
  let Some(bytes) = tree.get(key)? else {
    return Ok(None);
  };
  let Ok(entry) = serde_json::from_slice::<CacheEntry>(&bytes) else {
    return Ok(None);
  };

  Ok(cache_is_valid(root, config, &entry).then(|| restore_projects(root, entry.projects)))
}

pub(crate) fn store(
  cache: &sled::Db,
  root_octafile: &Path,
  root: &Path,
  config: &MonorepoConfig,
  result: &DiscoveryResult,
) -> Result<(), MonorepoError> {
  let tree = cache.open_tree(CACHE_TREE)?;
  let directories = result
    .directories
    .iter()
    .map(|directory| directory_stamp(root, directory))
    .collect::<Result<Vec<_>, _>>()?;
  let projects = result
    .projects
    .iter()
    .map(|project| CachedProject {
      namespace: project.namespace.clone(),
      octafile: project
        .octafile
        .strip_prefix(root)
        .ok()
        .map(path_text)
        .unwrap_or_else(|| path_text(&project.octafile)),
    })
    .collect();
  let entry = CacheEntry {
    version: CACHE_VERSION,
    config: config.clone(),
    directories,
    projects,
  };
  tree.insert(
    root_octafile.as_os_str().as_encoded_bytes(),
    serde_json::to_vec(&entry)?,
  )?;
  tree.flush()?;
  Ok(())
}

pub(crate) fn clear(cache: &sled::Db) -> Result<(), MonorepoError> {
  cache.drop_tree(CACHE_TREE)?;
  cache.flush()?;
  Ok(())
}

fn directory_stamp(root: &Path, directory: &Path) -> Result<DirectoryStamp, MonorepoError> {
  // A directory mtime changes when child entries are added, removed, or renamed.
  let modified = std::fs::metadata(directory)?
    .modified()?
    .duration_since(UNIX_EPOCH)
    .unwrap_or_default();
  Ok(DirectoryStamp {
    path: path_text(directory.strip_prefix(root).unwrap_or(Path::new(""))),
    modified_seconds: modified.as_secs(),
    modified_nanos: modified.subsec_nanos(),
  })
}

fn cache_is_valid(root: &Path, config: &MonorepoConfig, entry: &CacheEntry) -> bool {
  entry.version == CACHE_VERSION
    && &entry.config == config
    && entry.directories.iter().all(|expected| {
      directory_stamp(root, &root.join(&expected.path))
        .map(|actual| actual == *expected)
        .unwrap_or(false)
    })
    && entry
      .projects
      .iter()
      .all(|project| root.join(&project.octafile).is_file())
}

fn restore_projects(root: &Path, projects: Vec<CachedProject>) -> Vec<MonorepoProject> {
  projects
    .into_iter()
    .map(|project| MonorepoProject {
      namespace: project.namespace,
      octafile: root.join(project.octafile),
    })
    .collect()
}

fn path_text(path: &Path) -> String {
  path.to_string_lossy().replace('\\', "/")
}
