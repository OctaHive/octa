use std::fmt::{Display, Formatter};

use serde::{Deserialize, Deserializer, Serialize};
use serde_yml::Value;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ComplexCmd {
  pub task: String,
}

#[derive(Serialize, Debug, Clone)]
pub enum Cmds {
  Simple(String),
  Complex(ComplexCmd),
}

impl<'de> Deserialize<'de> for Cmds {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value: Value = Deserialize::deserialize(deserializer)?;

    if let Some(s) = value.as_str() {
      return Ok(Cmds::Simple(s.to_string()));
    }

    if let Some(map) = value.as_mapping() {
      if map.contains_key(&Value::String("task".to_string())) {
        let task = map.get(&Value::String("task".to_string()));
        if let Some(task) = task {
          return Ok(Cmds::Complex(ComplexCmd {
            task: task.as_str().unwrap().to_string(),
          }));
        }

        return Err(serde::de::Error::missing_field("task"));
      }
    }

    Err(serde::de::Error::custom("Invalid structure for Command"))
  }
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
