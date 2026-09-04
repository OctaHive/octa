use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

static NEXT_ALLOCATOR_ID: AtomicU64 = AtomicU64::new(0);

/// Identifies records produced by commands expanded from one task invocation.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize)]
pub struct ConsoleScope {
  #[serde(skip)]
  allocator_id: u64,
  id: u64,
  label: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  prefix: Option<String>,
}

impl ConsoleScope {
  pub fn id(&self) -> u64 {
    self.id
  }

  pub fn label(&self) -> &str {
    &self.label
  }

  /// Returns the configured output label or falls back to the task name.
  pub fn prefix(&self) -> &str {
    self.prefix.as_deref().unwrap_or(&self.label)
  }
}

/// Allocates ordered scope IDs independently from the selected renderer.
///
/// The allocator identity prevents collisions when independently built plans share a console.
#[derive(Debug)]
pub struct ConsoleScopeAllocator {
  id: u64,
  next_id: AtomicU64,
}

impl Default for ConsoleScopeAllocator {
  fn default() -> Self {
    Self {
      id: NEXT_ALLOCATOR_ID.fetch_add(1, Ordering::Relaxed),
      next_id: AtomicU64::new(0),
    }
  }
}

impl ConsoleScopeAllocator {
  pub fn scope(&self, label: impl Into<String>) -> ConsoleScope {
    self.scope_with_prefix(label, None)
  }

  pub fn scope_with_prefix(&self, label: impl Into<String>, prefix: Option<String>) -> ConsoleScope {
    ConsoleScope {
      allocator_id: self.id,
      id: self.next_id.fetch_add(1, Ordering::Relaxed),
      label: label.into(),
      prefix,
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn allocator_keeps_declaration_order() {
    let allocator = ConsoleScopeAllocator::default();
    assert_eq!(allocator.scope("build").id(), 0);
    assert_eq!(allocator.scope("test").id(), 1);
  }

  #[test]
  fn independent_allocators_produce_distinct_scope_identities() {
    let first = ConsoleScopeAllocator::default().scope("build");
    let second = ConsoleScopeAllocator::default().scope("build");

    assert_eq!(first.id(), second.id());
    assert_ne!(first, second);
  }

  #[test]
  fn serialized_scope_omits_the_internal_allocator_identity() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let value = serde_json::to_value(scope).unwrap();

    assert_eq!(value["id"], 0);
    assert_eq!(value["label"], "build");
    assert!(value.get("prefix").is_none());
    assert!(value.get("allocator_id").is_none());
  }

  #[test]
  fn custom_prefix_is_shared_and_serialized_with_the_scope() {
    let scope = ConsoleScopeAllocator::default().scope_with_prefix("build", Some("api".to_owned()));
    let value = serde_json::to_value(&scope).unwrap();

    assert_eq!(scope.label(), "build");
    assert_eq!(scope.prefix(), "api");
    assert_eq!(value["prefix"], "api");
  }
}
