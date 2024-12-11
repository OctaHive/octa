pub mod error;
pub mod executor;
mod function;
mod hash_source;
mod summary;
mod task;
mod timestamp_source;
pub mod vars;

use std::{env, path::PathBuf, sync::Arc};

use error::{ExecutorError, ExecutorResult};
pub use executor::Executor;
use octa_dag::DAG;
use octa_finder::{FindResult, OctaFinder};
use octa_octafile::{Cmds, Deps, ExecuteMode, Octafile, Task};
use task::CmdType;
pub use task::TaskNode;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use vars::Vars;

pub struct TaskGraphBuilder {
  finder: Arc<OctaFinder>, // Finder for search task in octafile
  dir: PathBuf,            // Current user directory
}

#[derive(Debug)]
struct DependencyInfo {
  task: Arc<TaskNode>,
  octafile: Arc<Octafile>,
  original_task: Task,
  path: String,
}

impl TaskGraphBuilder {
  pub fn new() -> ExecutorResult<Self> {
    let current_dir = env::current_dir()?;

    Ok(Self {
      finder: Arc::new(OctaFinder::new()),
      dir: current_dir,
    })
  }

  pub fn build(
    self,
    octafile: Arc<Octafile>,
    command: &str,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<DAG<TaskNode>> {
    debug!("Building DAG for command: {}", command);
    let mut dag = DAG::new();

    let commands = self.finder.find_by_path(Arc::clone(&octafile), command);
    if commands.is_empty() {
      return Err(ExecutorError::CommandNotFound(command.to_string()));
    }

    let found_commands: Vec<String> = commands.iter().map(|c| c.name.clone()).collect();
    debug!("Found commands for pattern {}: {:?}", command, found_commands);

    for cmd in commands {
      self.add_command_to_dag(&mut dag, cmd, cancel_token.clone())?;
    }

    if dag.node_count() == 0 {
      return Err(ExecutorError::TaskNotFound(command.to_string()));
    }

    if dag.has_cycle()? {
      return Err(ExecutorError::CycleDetected);
    }

    Ok(dag)
  }

  fn collect_vars(&self, cmd: &FindResult) -> Vars {
    let mut vars = Vars::new();
    let mut current = cmd.octafile.root();

    // Populate global variables
    vars.set_value(current.vars.clone());
    vars.insert("ROOT_DIR", &current.dir.display().to_string());
    vars.insert("TASKFILE_DIR", &current.dir.display().to_string());
    vars.insert("USER_WORKING_DIR", &self.dir.display().to_string());

    // Keep track of the current Arc
    #[allow(unused_assignments)]
    let mut current_arc: Option<Arc<Octafile>> = None;
    let full_path = cmd.octafile.hierarchy_path();

    debug!(
      "Collecting variables for command {} in path {}",
      cmd.name,
      full_path.join(":")
    );

    for segment in &full_path {
      debug!("Processing hierarchy segment: {}", segment);

      match current.get_included(segment).unwrap() {
        Some(nested_octafile) => {
          let mut new_vars = Vars::new();
          new_vars.set_parent(Some(vars));
          new_vars.set_value(nested_octafile.vars.clone());

          new_vars.insert("TASKFILE_DIR", &current.dir.display().to_string());

          // Store Arc and update current reference
          current_arc = Some(Arc::clone(&nested_octafile));
          current = current_arc.as_ref().unwrap();
          vars = new_vars;

          debug!("Updated variables for segment {}", segment);
        },
        None => {
          debug!("No nested octafile found for segment {}", segment);
          break;
        },
      }
    }

    // Add variables from current task
    let vars = match cmd.task.vars.clone() {
      Some(task_vars) => {
        let mut new_vars = Vars::new();
        new_vars.set_parent(Some(vars));
        new_vars.set_value(task_vars);

        new_vars
      },
      None => {
        let mut new_vars = Vars::new();
        new_vars.set_parent(Some(vars));

        new_vars
      },
    };

    debug!("Final collected variables: {:#?}", vars);
    vars
  }

  fn add_command_to_dag(
    &self,
    dag: &mut DAG<TaskNode>,
    cmd: FindResult,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<Arc<TaskNode>> {
    let nested_dag = match &cmd.task.cmds {
      Some(cmds) => {
        let mut nest_dag: DAG<TaskNode> = DAG::new();
        let mut prev_node = None;
        let mut index = 0;

        for command in cmds {
          match command {
            Cmds::Simple(_s) => {
              let task = cmd.task.clone();

              let nested_cmd = FindResult {
                name: format!("{}_{}", cmd.name.clone(), index),
                octafile: cmd.octafile.clone(),
                task: Task {
                  cmd: Some(command.clone()),
                  ..task
                },
              };

              let nested_task = Arc::new(self.create_task_from_command(
                &nested_cmd,
                None,
                cancel_token.clone(),
                None,
                Some(CmdType::Internal),
              ));

              nest_dag.add_node(Arc::clone(&nested_task));

              let should_add_dependency = cmd
                .task
                .execute_mode
                .as_ref()
                .map_or(true, |mode| mode == &ExecuteMode::Sequentially);

              if should_add_dependency {
                if let Some(prev) = &prev_node {
                  nest_dag.add_dependency(prev, &nested_task)?;
                }
              }

              prev_node = Some(nested_task);
              index = index + 1;
            },
            Cmds::Complex(c) => {
              let cmds = self.finder.find_by_path(Arc::clone(&cmd.octafile), &c.task);
              if cmds.is_empty() {
                return Err(ExecutorError::CommandNotFound(c.task.clone()));
              }

              for mut nested_cmd in cmds {
                if let Some(silent) = c.silent {
                  nested_cmd.task.silent = Some(silent);
                }

                let nested_task = Arc::new(self.create_task_from_command(
                  &nested_cmd,
                  None,
                  cancel_token.clone(),
                  c.vars.clone(),
                  Some(CmdType::Internal),
                ));

                nest_dag.add_node(Arc::clone(&nested_task));

                self.process_dependencies(
                  &mut nest_dag,
                  nested_cmd.octafile,
                  nested_task.clone(),
                  &nested_cmd.task,
                  nested_cmd.name,
                  cancel_token.clone(),
                )?;

                let should_add_dependency = cmd
                  .task
                  .execute_mode
                  .as_ref()
                  .map_or(true, |mode| mode == &ExecuteMode::Sequentially);

                if should_add_dependency {
                  if let Some(prev) = &prev_node {
                    nest_dag.add_dependency(prev, &nested_task)?;
                  }
                }
                prev_node = Some(nested_task);
              }
            },
          }
        }

        Some(nest_dag)
      },
      None => None,
    };

    let task = Arc::new(self.create_task_from_command(&cmd, nested_dag, cancel_token.clone(), None, None));
    let arc_task = Arc::clone(&task);

    dag.add_node(arc_task.clone());
    self.process_dependencies(dag, cmd.octafile, task, &cmd.task, cmd.name, cancel_token)?;

    Ok(arc_task)
  }

  /// Creates a task from a command with proper variable handling
  fn create_task_from_command(
    &self,
    cmd: &FindResult,
    dag: Option<DAG<TaskNode>>,
    cancel_token: CancellationToken,
    execute_vars: Option<octa_octafile::Vars>,
    cmd_type: Option<CmdType>,
  ) -> TaskNode {
    let task = cmd.task.clone();

    let mut vars = self.collect_vars(&cmd);
    vars.extend_with(&execute_vars);

    // Get task directory with fallback to taskfile directory
    let work_dir = task.dir.clone().or_else(|| Some(cmd.octafile.dir.clone())).unwrap();

    let command = match task.cmd {
      Some(cmd) => Some(cmd.to_string()),
      None => None,
    };

    TaskNode::new(
      cmd.name.clone(),
      work_dir,
      vars,
      cancel_token,
      command,
      task.tpl,
      task.sources,
      task.source_strategy,
      task.silent,
      task.ignore_error,
      task.run,
      dag,
      cmd_type,
    )
  }

  fn process_dependencies(
    &self,
    dag: &mut DAG<TaskNode>,
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
      self.add_dependency(dag, dep_info, &parent, cancel_token.clone())?;
    }

    Ok(())
  }

  fn collect_dependencies(
    &self,
    octafile: Arc<Octafile>,
    deps: &Vec<Deps>,
    curr_path: &str,
    cancel_token: CancellationToken,
  ) -> Vec<DependencyInfo> {
    deps
      .iter()
      .flat_map(|dep| match dep {
        Deps::Simple(dep) => self.finder.find_by_path(octafile.clone(), dep),
        Deps::Complex(c) => {
          let mut result = self.finder.find_by_path(octafile.clone(), &c.task);

          for res in &mut result {
            res.task.vars = match (res.task.vars.take(), &c.vars) {
              (Some(mut task_vars), Some(exec_vars)) => {
                task_vars.extend(exec_vars.clone());

                Some(task_vars)
              },
              (Some(task_vars), None) => Some(task_vars),
              (None, Some(exec_vars)) => Some(exec_vars.clone()),
              (None, None) => None,
            };

            if let Some(silent) = c.silent {
              res.task.silent = Some(silent);
            }
          }

          result
        },
      })
      .map(|cmd| {
        let cmd_name = self.join_path(curr_path, &cmd.name);

        let task = Arc::new(self.create_task_from_command(&cmd, None, cancel_token.clone(), None, None));

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
    &self,
    dag: &mut DAG<TaskNode>,
    dep_info: DependencyInfo,
    parent: &Arc<TaskNode>,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<()> {
    dag.add_node(Arc::clone(&dep_info.task));
    dag.add_dependency(&dep_info.task, parent)?;

    self.process_dependencies(
      dag,
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
