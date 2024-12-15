use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::octafile::Vars;

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum IncludeInfo {
  Simple(String),
  Complex(ComplexIncludeInfo),
}

/// Represents include directive information in octafile
#[derive(Serialize, Deserialize, Debug)]
pub struct ComplexIncludeInfo {
  pub octafile: String,
  pub optional: Option<bool>,
  pub dir: Option<PathBuf>,
  pub vars: Option<Vars>,
}
