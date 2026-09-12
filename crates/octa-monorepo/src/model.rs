use std::path::PathBuf;

/// One project Octafile selected by a monorepo root configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MonorepoProject {
  /// Colon-separated task namespace represented as individual path components.
  pub namespace: Vec<String>,
  /// Canonical path to the discovered Octafile.
  pub octafile: PathBuf,
}

/// Resolved Octafile entry point and projects visible to one invocation.
#[derive(Clone, Debug)]
pub struct MonorepoResolution {
  /// Octafile whose monorepo configuration owns this resolution.
  pub root_octafile: PathBuf,
  /// Discovered project containing the invocation directory, when any.
  pub current_namespace: Option<Vec<String>>,
  /// Projects to expose as synthetic includes of the root Octafile.
  pub projects: Vec<MonorepoProject>,
  /// Whether the project list was restored without traversing the workspace.
  pub cache_hit: bool,
}

pub(crate) struct DiscoveryResult {
  pub directories: Vec<PathBuf>,
  pub projects: Vec<MonorepoProject>,
}
