pub mod error;
mod executor;
mod function;
mod task;
pub mod vars;

use std::sync::Arc;

use error::{ExecutorError, ExecutorResult};
pub use executor::Executor;
use octa_dag::DAG;
use octa_finder::{FindResult, OctaFinder};
use octa_octafile::{Octafile, Task};
pub use task::TaskNode;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use vars::Vars;

pub struct TaskGraphBuilder {
  finder: Arc<OctaFinder>,
  dag: DAG<TaskNode>,
}

#[derive(Debug)]
struct DependencyInfo {
  task: Arc<TaskNode>,
  octafile: Arc<Octafile>,
  original_task: Task,
  path: String,
}

impl TaskGraphBuilder {
  pub fn new() -> Self {
    Self {
      finder: Arc::new(OctaFinder::new()),
      dag: DAG::new(),
    }
  }

  pub fn build(
    mut self,
    octafile: Arc<Octafile>,
    command: &str,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<DAG<TaskNode>> {
    debug!("Building DAG for command: {}", command);

    let commands = self.finder.find_by_path(Arc::clone(&octafile), command);
    if commands.is_empty() {
      return Err(ExecutorError::CommandNotFound(command.to_string()));
    }

    let found_commands: Vec<String> = commands.iter().map(|c| c.name.clone()).collect();
    debug!("Found commands for pattern {}: {:?}", command, found_commands);

    for cmd in commands {
      self.add_command_to_dag(cmd, cancel_token.clone())?;
    }

    if self.dag.node_count() == 0 {
      return Err(ExecutorError::TaskNotFound(command.to_string()));
    }

    if self.dag.has_cycle()? {
      return Err(ExecutorError::CycleDetected);
    }

    Ok(self.dag)
  }

  fn collect_vars(&self, cmd: &FindResult) -> Vars {
    let mut vars = Vars::new();
    let mut current = cmd.octafile.root();
    vars.set_value(current.vars.clone());

    debug!("Collecting variables for command: {}", cmd.name);

    // Store the current Arc to keep it alive through iterations
    #[allow(unused_assignments)]
    let mut current_arc = None;

    for segment in &cmd.octafile.hierarchy_path() {
      debug!("Processing hierarchy segment: {}", segment);

      match current.get_included(segment).unwrap() {
        Some(nested_octafile) => {
          let mut new_vars = Vars::new();
          new_vars.set_parent(Some(vars));
          new_vars.set_value(nested_octafile.vars.clone());

          // Store Arc and update current reference
          current_arc = Some(nested_octafile);
          current = current_arc.as_ref().unwrap().as_ref();
          vars = new_vars;

          debug!("Updated variables for segment {}", segment);
        },
        None => {
          debug!("No nested octafile found for segment {}", segment);
          break;
        },
      }
    }

    debug!("Final collected variables: {:#?}", vars);
    vars
  }

  fn add_command_to_dag(&mut self, cmd: FindResult, cancel_token: CancellationToken) -> ExecutorResult<()> {
    let task = Arc::new(self.create_task_from_command(&cmd, cancel_token.clone()));

    self.dag.add_node(Arc::clone(&task));
    self.process_dependencies(cmd.octafile, task, &cmd.task, cmd.name, cancel_token)
  }

  /// Creates a task from a command with proper variable handling
  fn create_task_from_command(&self, cmd: &FindResult, cancel_token: CancellationToken) -> TaskNode {
    let task = cmd.task.clone();

    let parent_vars = self.collect_vars(&cmd);

    let vars = match cmd.task.vars.clone() {
      Some(task_vars) => {
        let mut vars = Vars::new();
        vars.set_parent(Some(parent_vars));
        vars.set_value(task_vars);

        vars
      },
      None => {
        let mut vars = Vars::new();
        vars.set_parent(Some(parent_vars));

        vars
      },
    };

    // Get working directory with fallback to taskfile directory
    let work_dir = task.dir.clone().or_else(|| Some(cmd.octafile.dir.clone())).unwrap();

    TaskNode::new(cmd.name.clone(), task, work_dir, vars, cancel_token)
  }

  fn process_dependencies(
    &mut self,
    octafile: Arc<Octafile>,
    parent: Arc<TaskNode>,
    task: &Task,
    curr_path: String,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<()> {
    let Some(deps) = &task.deps else { return Ok(()) };
    if deps.is_empty() {
      return Ok(());
    };

    for dep_info in self.collect_dependencies(octafile, deps, &curr_path, cancel_token.clone()) {
      self.add_dependency(dep_info, &parent, cancel_token.clone())?;
    }

    Ok(())
  }

  fn collect_dependencies(
    &self,
    octafile: Arc<Octafile>,
    deps: &[String],
    curr_path: &str,
    cancel_token: CancellationToken,
  ) -> Vec<DependencyInfo> {
    deps
      .iter()
      .flat_map(|dep| self.finder.find_by_path(octafile.clone(), dep))
      .map(|cmd| {
        let cmd_name = self.join_path(curr_path, &cmd.name);

        let task = Arc::new(self.create_task_from_command(&cmd, cancel_token.clone()));

        DependencyInfo {
          task,
          octafile: cmd.octafile,
          original_task: cmd.task,
          path: cmd_name,
        }
      })
      .collect()
  }

  fn add_dependency(
    &mut self,
    dep_info: DependencyInfo,
    parent: &Arc<TaskNode>,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<()> {
    self.dag.add_node(Arc::clone(&dep_info.task));
    self.dag.add_dependency(&dep_info.task, parent)?;

    self.process_dependencies(
      dep_info.octafile,
      Arc::clone(&dep_info.task),
      &dep_info.original_task,
      dep_info.path,
      cancel_token,
    )
  }

  fn join_path(&self, current: &str, segment: &str) -> String {
    if current.is_empty() {
      segment.to_string()
    } else {
      format!("{}:{}", current, segment)
    }
  }
}
