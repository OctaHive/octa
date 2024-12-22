use humanize_duration::prelude::DurationExt;
use humanize_duration::Truncate;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use tracing::info;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummaryItem {
  pub name: String,
  pub duration: Duration,
}

#[derive(Debug)]
pub struct Summary {
  tasks: Mutex<Vec<TaskSummaryItem>>,
  total: Instant,
}

impl Default for Summary {
  fn default() -> Self {
    Self::new()
  }
}

impl Summary {
  pub fn new() -> Self {
    Self {
      tasks: Mutex::new(vec![]),
      total: Instant::now(),
    }
  }

  pub async fn add(&self, item: TaskSummaryItem) {
    let mut tasks = self.tasks.lock().await;
    tasks.push(item)
  }

  pub async fn print(&self) {
    let tasks = self.tasks.lock().await;
    info!("================== Time Summary ==================");
    for item in tasks.iter() {
      let human = item.duration.human(Truncate::Millis);
      info!(" \"{}\": {}", item.name, human);
    }
    info!(" Total time: {}", self.total.elapsed().human(Truncate::Millis));
    info!("==================================================");
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use tracing_test::traced_test;

  #[tokio::test]
  async fn test_new() {
    let summary = Summary::new();
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

  #[traced_test]
  #[tokio::test]
  async fn test_print() {
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

    summary.print().await;

    assert!(logs_contain("================== Time Summary =================="));
    assert!(logs_contain("\"task1\": 200ms"));
    assert!(logs_contain("\"task2\": 350ms"));
    assert!(logs_contain("Total time: 0ms"));
    assert!(logs_contain("=================================================="));
  }
}
