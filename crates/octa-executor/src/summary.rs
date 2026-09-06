//! Collection of successful task timings for the optional run summary.

use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummaryItem {
  /// Task label shown in the summary.
  pub name: String,
  /// Time spent executing the task node.
  pub duration: Duration,
}

/// Concurrent collector for task timings across one or more related runs.
#[derive(Debug)]
pub struct Summary {
  tasks: Mutex<Vec<TaskSummaryItem>>,
  total: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryReport {
  /// Successful task timings in completion order.
  pub tasks: Vec<TaskSummaryItem>,
  /// Elapsed time since the collector was created.
  pub total: Duration,
}

impl Default for Summary {
  fn default() -> Self {
    Self::new()
  }
}

impl Summary {
  /// Creates an empty timing collector and starts its total-duration clock.
  pub fn new() -> Self {
    Self {
      tasks: Mutex::new(vec![]),
      total: Instant::now(),
    }
  }

  /// Records a completed task.
  pub async fn add(&self, item: TaskSummaryItem) {
    let mut tasks = self.tasks.lock().await;
    tasks.push(item)
  }

  /// Returns presentation-neutral timing data for the selected console renderer.
  pub async fn report(&self) -> SummaryReport {
    SummaryReport {
      tasks: self.tasks.lock().await.clone(),
      total: self.total.elapsed(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn test_new() {
    let summary = Summary::default();
    let tasks = summary.tasks.lock().await;
    assert!(tasks.is_empty());
  }

  #[tokio::test]
  async fn test_add() {
    let summary = Summary::new();

    let duration1 = Duration::from_millis(200);
    let item1 = TaskSummaryItem {
      name: "task1".to_string(),
      duration: duration1,
    };
    summary.add(item1.clone()).await;
    let tasks = summary.tasks.lock().await;
    assert!(tasks.len() == 1);
    assert_eq!(tasks[0], item1);
    drop(tasks);

    let duration2 = Duration::from_millis(350);
    let item2 = TaskSummaryItem {
      name: "task2".to_string(),
      duration: duration2,
    };
    summary.add(item2.clone()).await;
    let tasks = summary.tasks.lock().await;
    assert!(tasks.len() == 2);
  }

  #[tokio::test]
  async fn test_report() {
    let summary = Summary::new();
    let duration1 = Duration::from_millis(200);
    let item1 = TaskSummaryItem {
      name: "task1".to_string(),
      duration: duration1,
    };
    summary.add(item1.clone()).await;
    let duration2 = Duration::from_millis(350);
    let item2 = TaskSummaryItem {
      name: "task2".to_string(),
      duration: duration2,
    };
    summary.add(item2.clone()).await;

    let report = summary.report().await;
    let later_report = summary.report().await;

    assert_eq!(report.tasks, vec![item1, item2]);
    assert!(later_report.total >= report.total);
  }
}
