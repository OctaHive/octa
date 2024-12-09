use humanize_duration::prelude::DurationExt;
use humanize_duration::Truncate;
use std::time::Duration;

use tracing::info;

#[derive(Debug)]
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
    info!("================== Time Summary ==================");
    for item in self.tasks.iter() {
      let human = item.duration.human(Truncate::Millis);
      info!("  \"{}\": \"{}\"", item.name, human);
    }
    info!("==================================================");
  }
}
