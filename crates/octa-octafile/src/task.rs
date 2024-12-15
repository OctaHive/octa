use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Cmds, Vars};

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ExecuteMode {
  Parallel,
  Sequentially,
}

impl From<String> for ExecuteMode {
  fn from(value: String) -> Self {
    match value.as_str() {
      "parallel" => ExecuteMode::Parallel,
      "sequentially" => ExecuteMode::Sequentially,
      _ => unimplemented!(),
    }
  }
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
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
pub enum SourceStrategies {
  Timestamp,
  Hash,
}

impl From<String> for SourceStrategies {
  fn from(value: String) -> Self {
    match value.as_str() {
      "timestamp" => SourceStrategies::Timestamp,
      "hash" => SourceStrategies::Hash,
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
pub struct ComplexDep {
  pub task: String,
  pub vars: Option<Vars>,
  pub silent: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Deps {
  Simple(String),
  Complex(ComplexDep),
}

impl From<String> for Deps {
  fn from(context: String) -> Self {
    Self::Simple(context)
  }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Task {
  pub dir: Option<PathBuf>,                      // Working directory for the task
  pub vars: Option<HashMap<String, String>>,     // Task-specific variables
  pub tpl: Option<String>,                       // Task template
  pub cmd: Option<Cmds>,                         // Command to execute
  pub cmds: Option<Vec<Cmds>>,                   // List of commands
  pub internal: Option<bool>,                    // Show command in list of available commands
  pub platforms: Option<Vec<AllowedPlatforms>>,  // Supported platforms
  pub ignore_error: Option<bool>,                // Whether to continue on error
  pub deps: Option<Vec<Deps>>,                   // Task dependencies
  pub run: Option<AllowedRun>,                   // When task should run
  pub silent: Option<bool>,                      // Should task print to stdout or stderr
  pub execute_mode: Option<ExecuteMode>,         // How execute task commands
  pub sources: Option<Vec<String>>,              // Sources for fingerprinting
  pub source_strategy: Option<SourceStrategies>, // Strategy for compare sources
  pub predonditions: Option<Vec<String>>,        // Commands to check should run command
}
