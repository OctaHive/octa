use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::Cmds;

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")] // Automatically match lowercase strings
pub enum AllowedPlatforms {
  Linux,
  Windows,
  Darwin,
}

impl From<String> for AllowedPlatforms {
  fn from(value: String) -> Self {
    match value.as_str() {
      "windows" => AllowedPlatforms::Windows,
      "darwin" => AllowedPlatforms::Darwin,
      "linux" => AllowedPlatforms::Linux,
      _ => unimplemented!(),
    }
  }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AllowedRun {
  Always,
  Once,
  Changed,
}

impl From<String> for AllowedRun {
  fn from(value: String) -> Self {
    match value.as_str() {
      "once" => AllowedRun::Once,
      "always" => AllowedRun::Always,
      "changed" => AllowedRun::Changed,
      _ => unimplemented!(),
    }
  }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Task {
  pub dir: Option<PathBuf>,                     // Working directory for the task
  pub vars: Option<HashMap<String, String>>,    // Task-specific variables
  pub tpl: Option<String>,                      // Task template
  pub cmd: Option<Cmds>,                        // Command to execute
  pub cmds: Option<Vec<Cmds>>,                  // List of commands
  pub internal: Option<bool>,                   // Show command in list of available commands
  pub platforms: Option<Vec<AllowedPlatforms>>, // Supported platforms
  pub ignore_error: Option<bool>,               // Whether to continue on error
  pub deps: Option<Vec<String>>,                // Task dependencies
  pub run: Option<AllowedRun>,                  // When task should run
}
