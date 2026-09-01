use std::path::{Path, PathBuf};

use octa_octafile::{read_monorepo_config, MonorepoConfig, Octafile};

use crate::MonorepoError;

pub(crate) fn find_root(
  entry_octafile: &Path,
  explicit_entry: bool,
) -> Result<Option<(PathBuf, MonorepoConfig)>, MonorepoError> {
  if explicit_entry {
    return Ok(read_monorepo_config(entry_octafile)?.map(|config| (entry_octafile.to_path_buf(), config)));
  }

  let entry_dir = entry_octafile.parent().expect("a canonical Octafile has a parent");
  for directory in entry_dir.ancestors() {
    let candidate = if directory == entry_dir {
      Some(entry_octafile.to_path_buf())
    } else {
      Octafile::find_in_directory(directory)
    };
    if let Some(path) = candidate {
      if let Some(config) = read_monorepo_config(&path)? {
        return Ok(Some((path.canonicalize()?, config)));
      }
    }
  }

  Ok(None)
}
