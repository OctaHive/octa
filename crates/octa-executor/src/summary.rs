use humanize_duration::prelude::DurationExt;
use humanize_duration::Truncate;
use std::time::Duration;

use tracing::info;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummaryItem {
  pub name: String,
  pub duration: Duration,
}

#[derive(Debug)]
pub struct Summary {
  tasks: Vec<TaskSummaryItem>,
}

impl Summary {
  pub fn new() -> Self {
    Self { tasks: vec![] }
  }

  pub fn add(&mut self, item: TaskSummaryItem) {
    self.tasks.push(item)
  }

  pub fn print(&self) {
    let mut total = Duration::new(0, 0);
    info!("================== Time Summary ==================");
    for item in self.tasks.iter() {
      total = total + item.duration;
      let human = item.duration.human(Truncate::Millis);
      info!(" \"{}\": {}", item.name, human);
    }
    info!(" Total time: {}", total.human(Truncate::Millis));
    info!("==================================================");
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use test_log::test;
  use tracing_test::traced_test;

  #[test]
  fn test_new() {
    let summary = Summary::new();
    assert!(summary.tasks.is_empty());
  }

  #[test]
  fn test_add() {
    let mut summary = Summary::new();
    let duration1 = Duration::from_millis(200);
    let item1 = TaskSummaryItem {
      name: "task1".to_string(),
      duration: duration1,
    };
    summary.add(item1.clone());
    assert!(summary.tasks.len() == 1);
    assert_eq!(summary.tasks[0], item1);

    let duration2 = Duration::from_millis(350);
    let item2 = TaskSummaryItem {
      name: "task2".to_string(),
      duration: duration2,
    };
    summary.add(item2.clone());
    assert!(summary.tasks.len() == 2);
  }

  #[traced_test]
  #[test]
  fn test_print() {
    let mut summary = Summary::new();
    let duration1 = Duration::from_millis(200);
    let item1 = TaskSummaryItem {
      name: "task1".to_string(),
      duration: duration1,
    };
    summary.add(item1.clone());
    let duration2 = Duration::from_millis(350);
    let item2 = TaskSummaryItem {
      name: "task2".to_string(),
      duration: duration2,
    };
    summary.add(item2.clone());

    summary.print();

    assert!(logs_contain("================== Time Summary =================="));
    assert!(logs_contain("\"task1\": 200ms"));
    assert!(logs_contain("\"task2\": 350ms"));
    assert!(logs_contain("Total time: 550ms"));
    assert!(logs_contain("=================================================="));
  }
}
