//! Plugin-owned model discovery metadata shared with embedding supervisors.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Capability advertised by plugins implementing explicit model discovery.
pub const MODEL_CATALOG_CAPABILITY_V1: &str = "model-catalog.v1";

/// Advisory plugin-owned runtime choices. Identifiers are never an execution allowlist.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(try_from = "ModelCatalogWire")]
pub struct ModelCatalog {
  /// Whether runtime discovery succeeded; an unavailable list may still allow manual entry.
  pub available: bool,
  /// Runtime-returned identifiers and per-model schemas, without credentials.
  pub models: Vec<ModelChoice>,
  /// JSON Schema for parameters of a manually entered or otherwise unlisted model.
  pub parameters: Map<String, Value>,
  /// Whether the runtime accepts a caller-supplied identifier outside the suggestions.
  pub manual_model: bool,
}

/// One model suggestion from the installed runtime, not a global product enum.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ModelChoice {
  /// Opaque identifier passed unchanged to this plugin.
  pub id: String,
  /// Runtime-provided display name.
  pub label: String,
  /// JSON Schema for this model's supported parameters.
  pub parameters: Map<String, Value>,
}

/// Maximum number of advisory model suggestions from one plugin.
pub const MAX_MODEL_CATALOG_ENTRIES: usize = 256;
/// Maximum UTF-8 bytes in an opaque runtime model identifier.
pub const MAX_MODEL_ID_BYTES: usize = 256;
/// Maximum encoded model metadata, below the existing transport frame ceiling.
pub const MAX_MODEL_CATALOG_BYTES: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCatalogWire {
  available: bool,
  models: Vec<ModelChoice>,
  parameters: Map<String, Value>,
  manual_model: bool,
}
impl TryFrom<ModelCatalogWire> for ModelCatalog {
  type Error = String;
  fn try_from(wire: ModelCatalogWire) -> Result<Self, Self::Error> {
    let catalog = Self {
      available: wire.available,
      models: wire.models,
      parameters: wire.parameters,
      manual_model: wire.manual_model,
    };
    catalog.validate()?;
    Ok(catalog)
  }
}
impl ModelCatalog {
  /// Rejects oversized or ambiguous plugin metadata before publication or consumption.
  pub fn validate(&self) -> Result<(), String> {
    let valid_name = |value: &str, limit| {
      !value.is_empty() && value.len() <= limit && value.trim() == value && !value.chars().any(char::is_control)
    };
    let mut names = std::collections::BTreeSet::new();
    if self.models.len() > MAX_MODEL_CATALOG_ENTRIES
      || self.models.iter().any(|model| {
        !valid_name(&model.id, MAX_MODEL_ID_BYTES) || !valid_name(&model.label, 512) || !names.insert(&model.id)
      })
      || serde_json::to_vec(self).map_err(|_| "invalid model catalog")?.len() > MAX_MODEL_CATALOG_BYTES
    {
      return Err("invalid or oversized model catalog".into());
    }
    Ok(())
  }
}
