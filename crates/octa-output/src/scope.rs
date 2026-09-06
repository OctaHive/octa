use std::{
  collections::HashMap,
  fmt,
  hash::{Hash, Hasher},
  sync::{
    atomic::{AtomicU64, Ordering},
    Arc, RwLock,
  },
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::RenderMode;

static NEXT_ALLOCATOR_ID: AtomicU64 = AtomicU64::new(1);

struct ScopeMetadata {
  prefix: RwLock<Option<String>>,
  render_mode: RwLock<Option<RenderMode>>,
  template_values: RwLock<HashMap<String, serde_json::Value>>,
  hide_stdout: bool,
  hide_stderr: bool,
}

/// Identifies records produced by commands expanded from one task invocation.
#[derive(Clone)]
pub struct ConsoleScope {
  allocator_id: u64,
  id: u64,
  parent_task_id: Option<u64>,
  label: String,
  metadata: Arc<ScopeMetadata>,
}

impl ConsoleScope {
  /// Execution-local task invocation identifier.
  pub fn id(&self) -> u64 {
    self.id
  }

  /// Human-readable invocation label; it is not a stable identifier.
  pub fn label(&self) -> &str {
    &self.label
  }

  /// Identifier of the task invocation that contains this task, when nested.
  pub fn parent_task_id(&self) -> Option<u64> {
    self.parent_task_id
  }

  pub fn prefix(&self) -> String {
    self
      .metadata
      .prefix
      .read()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
      .unwrap_or_else(|| self.label.clone())
  }

  pub fn set_prefix(&self, prefix: Option<String>) {
    *self
      .metadata
      .prefix
      .write()
      .unwrap_or_else(|poisoned| poisoned.into_inner()) = prefix;
  }

  pub fn render_mode(&self) -> Option<RenderMode> {
    *self
      .metadata
      .render_mode
      .read()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
  }

  pub fn set_render_mode(&self, mode: Option<RenderMode>) {
    *self
      .metadata
      .render_mode
      .write()
      .unwrap_or_else(|poisoned| poisoned.into_inner()) = mode;
  }

  pub fn template_values(&self) -> HashMap<String, serde_json::Value> {
    self
      .metadata
      .template_values
      .read()
      .unwrap_or_else(|poisoned| poisoned.into_inner())
      .clone()
  }

  pub fn set_template_values(&self, values: HashMap<String, serde_json::Value>) {
    *self
      .metadata
      .template_values
      .write()
      .unwrap_or_else(|poisoned| poisoned.into_inner()) = values;
  }

  pub fn hides_stdout(&self) -> bool {
    self.metadata.hide_stdout
  }

  pub fn hides_stderr(&self) -> bool {
    self.metadata.hide_stderr
  }
}

impl fmt::Debug for ConsoleScope {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("ConsoleScope")
      .field("id", &self.id)
      .field("parent_task_id", &self.parent_task_id)
      .field("label", &self.label)
      .field("prefix", &self.prefix())
      .field("render_mode", &self.render_mode())
      .finish()
  }
}

impl PartialEq for ConsoleScope {
  fn eq(&self, other: &Self) -> bool {
    self.allocator_id == other.allocator_id && self.id == other.id
  }
}

impl Eq for ConsoleScope {}

impl Hash for ConsoleScope {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.allocator_id.hash(state);
    self.id.hash(state);
  }
}

#[derive(Deserialize, Serialize)]
struct SerializedScope {
  id: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  parent_task_id: Option<u64>,
  label: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  prefix: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  output_mode: Option<RenderMode>,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  hide_stdout: bool,
  #[serde(default, skip_serializing_if = "std::ops::Not::not")]
  hide_stderr: bool,
}

impl Serialize for ConsoleScope {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    SerializedScope {
      id: self.id,
      parent_task_id: self.parent_task_id,
      label: self.label.clone(),
      prefix: self
        .metadata
        .prefix
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone(),
      output_mode: self.render_mode(),
      hide_stdout: self.metadata.hide_stdout,
      hide_stderr: self.metadata.hide_stderr,
    }
    .serialize(serializer)
  }
}

impl<'de> Deserialize<'de> for ConsoleScope {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: Deserializer<'de>,
  {
    let value = SerializedScope::deserialize(deserializer)?;
    Ok(Self {
      allocator_id: 0,
      id: value.id,
      parent_task_id: value.parent_task_id,
      label: value.label,
      metadata: Arc::new(ScopeMetadata {
        prefix: RwLock::new(value.prefix),
        render_mode: RwLock::new(value.output_mode),
        template_values: RwLock::new(HashMap::new()),
        hide_stdout: value.hide_stdout,
        hide_stderr: value.hide_stderr,
      }),
    })
  }
}

#[derive(Debug)]
pub struct ConsoleScopeAllocator {
  id: u64,
  next_id: AtomicU64,
  next_step_id: AtomicU64,
}

impl Default for ConsoleScopeAllocator {
  fn default() -> Self {
    Self {
      id: NEXT_ALLOCATOR_ID.fetch_add(1, Ordering::Relaxed),
      next_id: AtomicU64::new(0),
      next_step_id: AtomicU64::new(0),
    }
  }
}

impl ConsoleScopeAllocator {
  /// Allocates a root task invocation.
  pub fn scope(&self, label: impl Into<String>) -> ConsoleScope {
    self.scope_with_prefix(label, None)
  }

  /// Allocates a root task invocation with a static display prefix.
  pub fn scope_with_prefix(&self, label: impl Into<String>, prefix: Option<String>) -> ConsoleScope {
    self.scope_with_options(label, prefix, false, false)
  }

  /// Allocates a root task invocation with renderer and stream options.
  pub fn scope_with_options(
    &self,
    label: impl Into<String>,
    prefix: Option<String>,
    hide_stdout: bool,
    hide_stderr: bool,
  ) -> ConsoleScope {
    self.scope_with_parent_options(label, None, prefix, hide_stdout, hide_stderr)
  }

  /// Allocates a task invocation nested below another task invocation.
  pub fn scope_with_parent_options(
    &self,
    label: impl Into<String>,
    parent_task_id: Option<u64>,
    prefix: Option<String>,
    hide_stdout: bool,
    hide_stderr: bool,
  ) -> ConsoleScope {
    ConsoleScope {
      allocator_id: self.id,
      id: self.next_id.fetch_add(1, Ordering::Relaxed),
      parent_task_id,
      label: label.into(),
      metadata: Arc::new(ScopeMetadata {
        prefix: RwLock::new(prefix),
        render_mode: RwLock::new(None),
        template_values: RwLock::new(HashMap::new()),
        hide_stdout,
        hide_stderr,
      }),
    }
  }

  /// Allocates an executable step owned by `parent_task`.
  pub fn step(&self, parent_task: &ConsoleScope, label: impl Into<String>) -> ConsoleStep {
    ConsoleStep::new(self.next_step_id.fetch_add(1, Ordering::Relaxed), parent_task, label)
  }
}

/// Identifies one executable command inside a task invocation.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ConsoleStep {
  id: u64,
  parent_task_id: u64,
  label: String,
}

impl ConsoleStep {
  fn new(id: u64, parent_task: &ConsoleScope, label: impl Into<String>) -> Self {
    Self {
      id,
      parent_task_id: parent_task.id(),
      label: label.into(),
    }
  }

  /// Execution-local identifier, unique within the owning run.
  pub fn id(&self) -> u64 {
    self.id
  }

  /// Identifier of the task invocation that owns this step.
  pub fn parent_task_id(&self) -> u64 {
    self.parent_task_id
  }

  /// Human-readable plugin label; it is not a stable identifier.
  pub fn label(&self) -> &str {
    &self.label
  }
}

#[cfg(test)]
mod tests {
  use std::collections::hash_map::DefaultHasher;

  use super::*;

  #[test]
  fn allocator_keeps_declaration_order() {
    let allocator = ConsoleScopeAllocator::default();
    assert_eq!(allocator.scope("build").id(), 0);
    assert_eq!(allocator.scope("test").id(), 1);
  }

  #[test]
  fn nested_tasks_and_steps_expose_stable_parent_ids() {
    let allocator = ConsoleScopeAllocator::default();
    let parent = allocator.scope("build");
    let child = allocator.scope_with_parent_options("compile", Some(parent.id()), None, false, false);
    let step = allocator.step(&child, "shell");

    assert_eq!(child.parent_task_id(), Some(parent.id()));
    assert_eq!(step.id(), 0);
    assert_eq!(step.parent_task_id(), child.id());
    assert_eq!(step.label(), "shell");
    assert_eq!(serde_json::to_value(&child).unwrap()["parent_task_id"], parent.id());
  }

  #[test]
  fn independent_allocators_produce_distinct_scope_identities() {
    let first = ConsoleScopeAllocator::default().scope("build");
    let second = ConsoleScopeAllocator::default().scope("build");
    assert_eq!(first.id(), second.id());
    assert_ne!(first, second);
  }

  #[test]
  fn runtime_prefix_is_shared_by_scope_clones_and_serialized() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let clone = scope.clone();
    scope.set_prefix(Some("api".to_owned()));
    let value = serde_json::to_value(&clone).unwrap();
    assert_eq!(clone.prefix(), "api");
    assert_eq!(value["prefix"], "api");
    assert!(value.get("allocator_id").is_none());
  }

  #[test]
  fn template_values_are_shared_but_never_serialized() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    scope.set_template_values(HashMap::from([(
      "TOKEN".to_owned(),
      serde_json::Value::String("secret".to_owned()),
    )]));
    assert_eq!(scope.clone().template_values()["TOKEN"], "secret");
    let serialized = serde_json::to_string(&scope).unwrap();
    assert!(!serialized.contains("TOKEN"));
    assert!(!serialized.contains("secret"));
  }

  #[test]
  fn options_render_mode_debug_and_hash_are_stable() {
    let scope = ConsoleScopeAllocator::default().scope_with_options("build", Some("api".to_owned()), true, false);
    scope.set_render_mode(Some(RenderMode::KeepOrder));

    assert_eq!(scope.label(), "build");
    assert_eq!(scope.prefix(), "api");
    assert_eq!(scope.render_mode(), Some(RenderMode::KeepOrder));
    assert!(scope.hides_stdout());
    assert!(!scope.hides_stderr());
    assert!(format!("{scope:?}").contains("KeepOrder"));

    let mut first = DefaultHasher::new();
    let mut second = DefaultHasher::new();
    scope.hash(&mut first);
    scope.clone().hash(&mut second);
    assert_eq!(first.finish(), second.finish());
  }

  #[test]
  fn serialized_scope_round_trips_public_metadata() {
    let original = ConsoleScopeAllocator::default().scope_with_options("build", Some("api".to_owned()), true, true);
    original.set_render_mode(Some(RenderMode::Timed));
    let restored: ConsoleScope = serde_json::from_value(serde_json::to_value(original).unwrap()).unwrap();

    assert_eq!(restored.id(), 0);
    assert_eq!(restored.label(), "build");
    assert_eq!(restored.prefix(), "api");
    assert_eq!(restored.render_mode(), Some(RenderMode::Timed));
    assert!(restored.hides_stdout());
    assert!(restored.hides_stderr());
    assert!(restored.template_values().is_empty());
  }
}
