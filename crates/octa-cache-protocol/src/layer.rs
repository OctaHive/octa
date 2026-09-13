//! Stable provenance of an action result selected by a cache store.

use serde::{Deserialize, Serialize};

/// Physical cache tier that supplied validated action-result metadata.
///
/// The value belongs to the shared protocol because it is emitted through
/// structured task results and runner events, while store implementations use
/// it to retain provenance across layered lookups. Output blobs are verified
/// later and do not change this metadata provenance.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum CacheLayer {
  /// Cache colocated with the current Octa process or agent.
  Local,
  /// Cache reached through a remote service.
  Remote,
}
