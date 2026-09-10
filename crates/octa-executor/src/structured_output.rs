//! Bounded structured values shared by task results, dependencies, and caches.

use std::{
  collections::HashSet,
  fmt,
  io::{self, Write},
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  },
};

use octa_output::{RegisteredArtifact, RegisteredReport};
use serde_json::{Map, Value};

use crate::error::{ExecutorError, ExecutorResult};

/// Maximum serialized structured output retained by one execution.
pub(crate) const MAX_STRUCTURED_OUTPUT_BYTES: usize = 64 * 1024 * 1024;

/// Shared run-level accounting for structured plugin results.
#[derive(Debug)]
pub(crate) struct StructuredOutputBudget {
  used: AtomicUsize,
  limit: usize,
}

impl Default for StructuredOutputBudget {
  fn default() -> Self {
    Self::new(MAX_STRUCTURED_OUTPUT_BYTES)
  }
}

impl StructuredOutputBudget {
  pub(crate) fn new(limit: usize) -> Self {
    Self {
      used: AtomicUsize::new(0),
      limit,
    }
  }

  /// Accounts for one logical step result before it is retained by the executor.
  pub(crate) fn reserve(&self, outputs: &CompletionOutputs) -> ExecutorResult<()> {
    let step_size = serialized_size(outputs.step())?;
    let task_size = serialized_size(&outputs.task().0.values)?;
    let size = step_size.saturating_add(task_size);
    self
      .used
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |used| {
        used.checked_add(size).filter(|total| *total <= self.limit)
      })
      .map(|_| ())
      .map_err(|_| ExecutorError::StructuredOutputLimitExceeded { limit: self.limit })
  }
}

fn serialized_size(outputs: &Map<String, Value>) -> ExecutorResult<usize> {
  let mut counter = ByteCounter::default();
  serde_json::to_writer(&mut counter, outputs)
    .map_err(|error| ExecutorError::StructuredOutputSerializationFailed(error.to_string()))?;
  Ok(counter.bytes)
}

/// Counts serialized bytes without allocating a second JSON buffer.
#[derive(Default)]
struct ByteCounter {
  bytes: usize,
}

impl Write for ByteCounter {
  fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
    self.bytes = self
      .bytes
      .checked_add(buffer.len())
      .ok_or_else(|| io::Error::other("structured output size overflow"))?;
    Ok(buffer.len())
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

/// Task exports retained internally together with their redaction metadata.
#[derive(Clone, Default, Eq, PartialEq)]
pub(crate) struct TaskOutputs(Arc<TaskOutputData>);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct TaskOutputData {
  values: Map<String, Value>,
  secrets: HashSet<String>,
}

impl TaskOutputs {
  pub(crate) fn insert(&mut self, name: String, value: Value, secret: bool) {
    let data = Arc::make_mut(&mut self.0);
    data.values.insert(name.clone(), value);
    if secret {
      data.secrets.insert(name);
    } else {
      data.secrets.remove(&name);
    }
  }

  pub(crate) fn get(&self, name: &str) -> Option<&Value> {
    self.0.values.get(name)
  }

  pub(crate) fn is_secret(&self, name: &str) -> bool {
    self.0.secrets.contains(name)
  }

  /// Merges later step values into an invocation result.
  pub(crate) fn extend(&mut self, newer: &Self) {
    let data = Arc::make_mut(&mut self.0);
    for (name, value) in &newer.0.values {
      data.values.insert(name.clone(), value.clone());
      if newer.0.secrets.contains(name) {
        data.secrets.insert(name.clone());
      } else {
        data.secrets.remove(name);
      }
    }
  }

  /// Restores outputs accumulated by earlier nodes without replacing newer values.
  pub(crate) fn extend_missing(&mut self, older: &Self) {
    let data = Arc::make_mut(&mut self.0);
    for (name, value) in &older.0.values {
      if data.values.contains_key(name) {
        continue;
      }
      data.values.insert(name.clone(), value.clone());
      if older.0.secrets.contains(name) {
        data.secrets.insert(name.clone());
      }
    }
  }

  /// Returns only values safe to persist or serialize in the public result.
  pub(crate) fn public_values(&self) -> Map<String, Value> {
    self
      .0
      .values
      .iter()
      .filter(|(name, _)| !self.0.secrets.contains(*name))
      .map(|(name, value)| (name.clone(), value.clone()))
      .collect()
  }

  pub(crate) fn secret_names(&self) -> Vec<String> {
    let mut names = self.0.secrets.iter().cloned().collect::<Vec<_>>();
    names.sort();
    names
  }
}

impl fmt::Debug for TaskOutputs {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("TaskOutputs")
      .field("public", &PublicTaskOutputs(&self.0))
      .field("redacted_count", &self.0.secrets.len())
      .finish()
  }
}

/// Borrowed debug view that never clones values or formats secret entries.
struct PublicTaskOutputs<'a>(&'a TaskOutputData);

impl fmt::Debug for PublicTaskOutputs<'_> {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut map = formatter.debug_map();
    for (name, value) in &self.0.values {
      if !self.0.secrets.contains(name) {
        map.entry(name, value);
      }
    }
    map.finish()
  }
}

/// Structured data produced by one graph node.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct CompletionOutputs {
  step: Arc<Map<String, Value>>,
  task: TaskOutputs,
  artifacts: Arc<Vec<RegisteredArtifact>>,
  reports: Arc<Vec<RegisteredReport>>,
}

/// One public task output selected from a plugin step result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StepExport {
  pub(crate) field: String,
  pub(crate) secret: bool,
}

impl CompletionOutputs {
  pub(crate) fn new(step: Map<String, Value>, task: TaskOutputs) -> Self {
    Self {
      step: Arc::new(step),
      task,
      artifacts: Arc::new(Vec::new()),
      reports: Arc::new(Vec::new()),
    }
  }

  pub(crate) fn with_resources(mut self, artifacts: Vec<RegisteredArtifact>, reports: Vec<RegisteredReport>) -> Self {
    self.artifacts = Arc::new(artifacts);
    self.reports = Arc::new(reports);
    self
  }

  pub(crate) fn step(&self) -> &Map<String, Value> {
    &self.step
  }

  pub(crate) fn shared_step(&self) -> Arc<Map<String, Value>> {
    Arc::clone(&self.step)
  }

  pub(crate) fn task(&self) -> &TaskOutputs {
    &self.task
  }

  pub(crate) fn into_task(self) -> TaskOutputs {
    self.task
  }

  pub(crate) fn artifacts(&self) -> &[RegisteredArtifact] {
    &self.artifacts
  }

  pub(crate) fn reports(&self) -> &[RegisteredReport] {
    &self.reports
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn secret_values_remain_internal_and_merge_with_their_metadata() {
    let mut earlier = TaskOutputs::default();
    earlier.insert("token".to_owned(), json!("secret"), true);
    earlier.insert("image".to_owned(), json!("old"), false);
    let mut newer = TaskOutputs::default();
    newer.insert("image".to_owned(), json!("new"), true);
    newer.extend_missing(&earlier);

    assert_eq!(newer.get("token"), Some(&json!("secret")));
    assert!(newer.is_secret("token"));
    assert_eq!(newer.get("image"), Some(&json!("new")));
    assert!(newer.is_secret("image"));
    assert!(newer.public_values().is_empty());
    assert_eq!(newer.secret_names(), ["image", "token"]);
    assert!(!format!("{newer:?}").contains("secret"));

    let mut public = TaskOutputs::default();
    public.insert("image".to_owned(), json!("public"), false);
    newer.extend(&public);
    assert_eq!(newer.public_values()["image"], "public");
    assert!(!newer.is_secret("image"));
  }

  #[test]
  fn budget_counts_step_and_exported_values_without_partial_reservations() {
    let mut task = TaskOutputs::default();
    task.insert("result".to_owned(), json!("value"), false);
    let outputs = CompletionOutputs::new(Map::from_iter([("step".to_owned(), json!("value"))]), task);
    let size = serialized_size(outputs.step()).unwrap() + serialized_size(&outputs.task().0.values).unwrap();
    let budget = StructuredOutputBudget::new(size);

    budget.reserve(&outputs).unwrap();
    assert!(matches!(
      budget.reserve(&outputs),
      Err(ExecutorError::StructuredOutputLimitExceeded { limit }) if limit == size
    ));
  }
}
