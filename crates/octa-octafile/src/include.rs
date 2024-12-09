use serde::{Deserialize, Serialize};

/// Represents include directive information in taskfile
#[derive(Serialize, Deserialize, Debug)]
pub struct IncludeInfo {
  pub octafile: String,
  pub optional: Option<bool>,
}
