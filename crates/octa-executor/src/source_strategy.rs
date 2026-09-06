//! Pluggable source fingerprint strategies and their executor registry.

use std::{collections::HashMap, fmt, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use octa_octafile::SourceStrategies;
use tokio_util::sync::CancellationToken;

use crate::error::{ExecutorError, ExecutorResult};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub enum SourceMethod {
  Timestamp,
  Hash,
  Custom(String),
}

impl SourceMethod {
  #[cfg(test)]
  pub fn custom(name: impl Into<String>) -> Self {
    Self::Custom(name.into())
  }

  pub fn as_str(&self) -> &str {
    match self {
      Self::Timestamp => "timestamp",
      Self::Hash => "hash",
      Self::Custom(name) => name,
    }
  }
}

#[async_trait]
pub trait SourceStrategy: Send + Sync {
  /// Stable identifier included in persisted fingerprint keys.
  fn key(&self) -> &'static str;

  /// Whether output timestamps should be compared with source timestamps.
  fn compare_output_timestamps(&self) -> bool {
    false
  }

  /// Produces a stable fingerprint for sorted source paths expanded by the collector.
  async fn fingerprint(&self, sources: &[PathBuf], cancel_token: &CancellationToken) -> ExecutorResult<Vec<u8>>;
}

#[derive(Clone)]
pub(crate) struct SourceStrategyHandle(Arc<dyn SourceStrategy>);

impl SourceStrategyHandle {
  pub(crate) fn new<S>(strategy: S) -> Self
  where
    S: SourceStrategy + 'static,
  {
    Self(Arc::new(strategy))
  }

  pub(crate) fn key(&self) -> &'static str {
    self.0.key()
  }

  pub(crate) fn compare_output_timestamps(&self) -> bool {
    self.0.compare_output_timestamps()
  }

  pub(crate) async fn fingerprint(
    &self,
    sources: &[PathBuf],
    cancel_token: &CancellationToken,
  ) -> ExecutorResult<Vec<u8>> {
    self.0.fingerprint(sources, cancel_token).await
  }
}

impl fmt::Debug for SourceStrategyHandle {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_tuple("SourceStrategy").field(&self.key()).finish()
  }
}

#[derive(Clone)]
pub(crate) struct SourceStrategyRegistry {
  strategies: HashMap<SourceMethod, SourceStrategyHandle>,
}

impl SourceStrategyRegistry {
  pub(crate) fn register<S>(&mut self, method: SourceMethod, strategy: S)
  where
    S: SourceStrategy + 'static,
  {
    self.strategies.insert(method, SourceStrategyHandle::new(strategy));
  }

  pub(crate) fn resolve(&self, method: &SourceMethod) -> ExecutorResult<SourceStrategyHandle> {
    self
      .strategies
      .get(method)
      .cloned()
      .ok_or_else(|| ExecutorError::SourceStrategyUnavailable(method.as_str().to_owned()))
  }
}

impl Default for SourceStrategyRegistry {
  fn default() -> Self {
    let mut registry = Self {
      strategies: HashMap::new(),
    };
    registry.register(SourceMethod::Timestamp, crate::timestamp_source::TimestampSource);
    registry.register(SourceMethod::Hash, crate::hash_source::HashSource);
    registry
  }
}

impl From<SourceStrategies> for SourceMethod {
  fn from(value: SourceStrategies) -> Self {
    match value {
      SourceStrategies::Timestamp => SourceMethod::Timestamp,
      SourceStrategies::Hash => SourceMethod::Hash,
      SourceStrategies::Custom(name) => SourceMethod::Custom(name),
    }
  }
}
