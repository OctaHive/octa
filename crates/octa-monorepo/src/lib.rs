use std::path::Path;

mod cache;
mod discovery;
mod error;
mod model;
mod root;

#[cfg(test)]
mod tests;

pub use error::MonorepoError;
pub use model::{MonorepoProject, MonorepoResolution};

/// Resolves the active monorepo root and discovers its project Octafiles.
pub fn resolve(
  entry_octafile: &Path,
  working_dir: &Path,
  explicit_entry: bool,
  cache: &sled::Db,
) -> Result<MonorepoResolution, MonorepoError> {
  let entry_octafile = entry_octafile.canonicalize()?;
  let Some((root_octafile, config)) = root::find_root(&entry_octafile, explicit_entry)? else {
    return Ok(standalone(entry_octafile));
  };
  config.validate().map_err(MonorepoError::InvalidConfiguration)?;

  let root_dir = root_octafile.parent().expect("a canonical Octafile has a parent");
  let (projects, cache_hit) = match cache::load(cache, &root_octafile, root_dir, &config)? {
    Some(projects) => (projects, true),
    None => {
      let result = discovery::discover(root_dir, &root_octafile, &config)?;
      cache::store(cache, &root_octafile, root_dir, &config, &result)?;
      (result.projects, false)
    },
  };

  if entry_octafile != root_octafile && !projects.iter().any(|project| project.octafile == entry_octafile) {
    // Discovery must not hide a local Octafile that the root configuration does not select.
    return Ok(standalone(entry_octafile));
  }

  let current_namespace = current_namespace(working_dir, &projects);
  Ok(MonorepoResolution {
    root_octafile,
    current_namespace,
    projects,
    cache_hit,
  })
}

/// Clears the cached monorepo discovery manifests stored in the supplied database.
pub fn clear_cache(cache: &sled::Db) -> Result<(), MonorepoError> {
  cache::clear(cache)
}

fn standalone(root_octafile: std::path::PathBuf) -> MonorepoResolution {
  MonorepoResolution {
    root_octafile,
    current_namespace: None,
    projects: Vec::new(),
    cache_hit: false,
  }
}

fn current_namespace(working_dir: &Path, projects: &[MonorepoProject]) -> Option<Vec<String>> {
  let working_dir = working_dir.canonicalize().ok()?;
  projects
    .iter()
    .filter_map(|project| {
      let directory = project.octafile.parent()?;
      working_dir
        .starts_with(directory)
        .then_some((directory.components().count(), project.namespace.clone()))
    })
    .max_by_key(|(depth, _)| *depth)
    .map(|(_, namespace)| namespace)
}
