use std::{fmt, str::FromStr};

use serde::{de, ser::SerializeMap, Deserialize, Deserializer, Serialize, Serializer};

/// Presentation style selected for runtime task output.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputMode {
  #[default]
  Interleaved,
  Group,
  Prefixed,
  OnError,
  KeepOrder,
  Replacing,
  Timed,
  Json,
}

impl OutputMode {
  pub const VALUES: &'static [&'static str] = &[
    "interleaved",
    "group",
    "prefixed",
    "on-error",
    "keep-order",
    "replacing",
    "timed",
    "json",
  ];
}

impl FromStr for OutputMode {
  type Err = String;

  fn from_str(value: &str) -> Result<Self, Self::Err> {
    match value {
      "interleaved" | "interleave" => Ok(Self::Interleaved),
      "group" => Ok(Self::Group),
      "prefixed" | "prefix" => Ok(Self::Prefixed),
      "on-error" => Ok(Self::OnError),
      "keep-order" => Ok(Self::KeepOrder),
      "replacing" => Ok(Self::Replacing),
      "timed" => Ok(Self::Timed),
      "json" | "jsonl" => Ok(Self::Json),
      _ => Err(format!(
        "unknown output mode '{value}'; expected one of {}",
        Self::VALUES.join(", ")
      )),
    }
  }
}

impl<'de> Deserialize<'de> for OutputMode {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    String::deserialize(deserializer)?.parse().map_err(de::Error::custom)
  }
}

impl fmt::Display for OutputMode {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let value = match self {
      Self::Interleaved => "interleaved",
      Self::Group => "group",
      Self::Prefixed => "prefixed",
      Self::OnError => "on-error",
      Self::KeepOrder => "keep-order",
      Self::Replacing => "replacing",
      Self::Timed => "timed",
      Self::Json => "json",
    };
    formatter.write_str(value)
  }
}

/// Human presentation modes that can be selected for a single task. JSON is
/// intentionally root-only because a JSONL stream cannot be mixed with text.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskOutputMode {
  Interleaved,
  Group,
  Prefixed,
  OnError,
  KeepOrder,
  Replacing,
  Timed,
}

impl<'de> Deserialize<'de> for TaskOutputMode {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    match OutputMode::deserialize(deserializer)? {
      OutputMode::Interleaved => Ok(Self::Interleaved),
      OutputMode::Group => Ok(Self::Group),
      OutputMode::Prefixed => Ok(Self::Prefixed),
      OutputMode::OnError => Ok(Self::OnError),
      OutputMode::KeepOrder => Ok(Self::KeepOrder),
      OutputMode::Replacing => Ok(Self::Replacing),
      OutputMode::Timed => Ok(Self::Timed),
      OutputMode::Json => Err(de::Error::custom("JSON output can only be selected globally")),
    }
  }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TaskPresentation {
  pub output: Option<TaskOutputMode>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupOutput {
  pub begin: Option<String>,
  pub end: Option<String>,
  #[serde(default)]
  pub error_only: bool,
}

/// Root-level output configuration. The mapping form intentionally follows the
/// familiar Taskfile syntax: `output: { group: { begin, end, error_only } }`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct OutputConfig {
  pub mode: OutputMode,
  pub group: GroupOutput,
}

impl Serialize for OutputConfig {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    if self.mode != OutputMode::Group {
      return self.mode.serialize(serializer);
    }
    let mut mapping = serializer.serialize_map(Some(1))?;
    mapping.serialize_entry("group", &self.group)?;
    mapping.end()
  }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OutputConfigValue {
  Mode(OutputMode),
  Detailed(OutputConfigMapping),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputConfigMapping {
  group: GroupOutput,
}

impl<'de> Deserialize<'de> for OutputConfig {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    match OutputConfigValue::deserialize(deserializer)? {
      OutputConfigValue::Mode(mode) => Ok(Self {
        mode,
        group: GroupOutput::default(),
      }),
      OutputConfigValue::Detailed(value) => Ok(Self {
        mode: OutputMode::Group,
        group: value.group,
      }),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn parses_simple_and_detailed_output_configuration() {
    let simple: OutputConfig = serde_yml::from_str("keep-order").unwrap();
    assert_eq!(simple.mode, OutputMode::KeepOrder);

    let detailed: OutputConfig =
      serde_yml::from_str("group:\n  begin: '::group::{{.TASK}}'\n  end: '::endgroup::'\n  error_only: true").unwrap();
    assert_eq!(detailed.mode, OutputMode::Group);
    assert_eq!(detailed.group.begin.as_deref(), Some("::group::{{.TASK}}"));
    assert!(detailed.group.error_only);
  }

  #[test]
  fn accepts_mise_aliases_but_serializes_canonical_names() {
    assert_eq!("prefix".parse(), Ok(OutputMode::Prefixed));
    assert_eq!("interleave".parse(), Ok(OutputMode::Interleaved));
    assert_eq!("timed".parse(), Ok(OutputMode::Timed));
    assert_eq!(
      serde_yml::to_string(&OutputMode::KeepOrder).unwrap().trim(),
      "keep-order"
    );
  }

  #[test]
  fn task_output_modes_accept_aliases_but_reject_json() {
    assert_eq!(
      serde_yml::from_str::<TaskOutputMode>("prefix").unwrap(),
      TaskOutputMode::Prefixed
    );
    assert_eq!(
      serde_yml::from_str::<TaskOutputMode>("interleave").unwrap(),
      TaskOutputMode::Interleaved
    );
    assert!(serde_yml::from_str::<TaskOutputMode>("json").is_err());
  }

  #[test]
  fn output_configuration_round_trips_through_yaml() {
    for config in [
      OutputConfig {
        mode: OutputMode::KeepOrder,
        group: GroupOutput::default(),
      },
      OutputConfig {
        mode: OutputMode::Group,
        group: GroupOutput {
          begin: Some("begin".to_owned()),
          end: Some("end".to_owned()),
          error_only: true,
        },
      },
    ] {
      let yaml = serde_yml::to_string(&config).unwrap();
      assert_eq!(serde_yml::from_str::<OutputConfig>(&yaml).unwrap(), config);
    }
  }
}
