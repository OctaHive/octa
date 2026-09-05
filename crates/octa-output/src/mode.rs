use serde::{Deserialize, Serialize};

/// Concrete presentation style used by the output router.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RenderMode {
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
