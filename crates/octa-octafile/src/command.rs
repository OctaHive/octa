use std::fmt::{Display, Formatter};

use serde::{Deserialize, Serialize};

use crate::octafile::{Envs, Vars};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ComplexCmd {
  pub task: String,
  pub vars: Option<Vars>,
  pub envs: Option<Envs>,
  pub silent: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum Cmds {
  Simple(String),
  Complex(ComplexCmd),
}

impl From<String> for Cmds {
  fn from(context: String) -> Self {
    Self::Simple(context)
  }
}

impl Display for Cmds {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    match self {
      Cmds::Simple(s) => writeln!(f, "{}", s),
      Cmds::Complex(c) => writeln!(f, "{}", c.task),
    }
  }
}
