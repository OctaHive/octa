pub mod envs;
pub mod error;
pub mod executor;
mod function;
mod hash_source;
pub mod summary;
mod task;
mod timestamp_source;
pub mod vars;

use std::{env, future::Future, path::PathBuf, pin::Pin, sync::Arc};

use async_stream::stream;
use envs::Envs;
use futures::StreamExt;
use tokio_stream::iter;
use tokio_util::sync::CancellationToken;
use tracing::debug;

use error::{ExecutorError, ExecutorResult};
pub use executor::Executor;
use octa_dag::DAG;
use octa_finder::{FindResult, OctaFinder};
use octa_octafile::{Cmds, ComplexCmd, Deps, ExecuteMode, Octafile, Task};
pub use task::TaskNode;
use task::{CmdType, TaskConfig};
use vars::Vars;

type DagNode = DAG<TaskNode>;

pub struct TaskGraphBuilder {
  finder: Arc<OctaFinder>,   // Finder for search task in octafile
  dir: PathBuf,              // Current user directory
  command_args: Vec<String>, // Aditional task arguments from cli
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
      command_args: vec![],
    })
  }

  pub async fn build(
    mut self,
    octafile: Arc<Octafile>,
    command: &str,
    cancel_token: CancellationToken,
    command_args: Vec<String>,
  ) -> ExecutorResult<DAG<TaskNode>> {
    debug!(
      "Building DAG for command {} with provided args {:?}",
      command, command_args
    );
    self.command_args = command_args;
    let mut dag = DAG::new();

    let commands = self.finder.find_by_path(Arc::clone(&octafile), command);
    if commands.is_empty() {
      return Err(ExecutorError::CommandNotFound(command.to_string()));
    }

    let found_commands: Vec<String> = commands.iter().map(|c| c.name.clone()).collect();
    debug!("Found commands for pattern {}: {:?}", command, found_commands);

    for cmd in commands {
      self.add_command_to_dag(&mut dag, cmd, cancel_token.clone()).await?;
    }

    self.validate_dag(&dag, command)?;

    Ok(dag)
  }

  fn initialize_global_vars(&self, cmd: &FindResult) -> Vars {
    let mut vars = Vars::new();
    let root = cmd.octafile.root();

    vars.set_value(root.vars.clone());

    vars.insert("ROOT_DIR", &root.dir.display().to_string());
    vars.insert("TASKFILE_DIR", &root.dir.display().to_string());
    vars.insert("USER_WORKING_DIR", &self.dir.display().to_string());
    vars.insert("COMMAND_ARGS", &self.command_args);

    vars
  }

  fn initialize_global_envs(&self, cmd: &FindResult) -> Envs {
    let mut envs = Envs::new();
    let root = cmd.octafile.root();

    if let Some(env) = &root.env {
      envs.set_value(env.clone());
    }

    envs
  }

  fn process_hierarchy_vars(&self, cmd: &FindResult, vars: &mut Vars) {
    let full_path = cmd.octafile.hierarchy_path();
    let mut current = Arc::clone(cmd.octafile.root());

    debug!(
      "Processing hierarchy variables for command {} in path {}",
      cmd.name,
      full_path.join(":")
    );

    for segment in full_path {
      match current.get_included(&segment).unwrap() {
        Some(nested_octafile) => {
          let mut new_vars = Vars::new();
          new_vars.set_parent(Some(vars.clone()));
          new_vars.set_value(nested_octafile.vars.clone());
          new_vars.insert("TASKFILE_DIR", &current.dir.display().to_string());

          *vars = new_vars;
          current = Arc::clone(&nested_octafile);
          debug!("Updated variables for segment {}", segment);
        },
        None => {
          debug!("No nested octafile found for segment {}", segment);
          break;
        },
      }
    }
  }

  fn process_hierarchy_envs(&self, cmd: &FindResult, envs: &mut Envs) {
    let full_path = cmd.octafile.hierarchy_path();
    let mut current = Arc::clone(cmd.octafile.root());

    debug!(
      "Processing hierarchy environments for command {} in path {}",
      cmd.name,
      full_path.join(":")
    );

    for segment in full_path {
      match current.get_included(&segment).unwrap() {
        Some(nested_octafile) => {
          let mut new_envs = Envs::new();
          new_envs.set_parent(Some(envs.clone()));
          if let Some(env) = &nested_octafile.env {
            new_envs.set_value(env.clone());
          }

          *envs = new_envs;
          current = Arc::clone(&nested_octafile);
          debug!("Updated environments for segment {}", segment);
        },
        None => {
          debug!("No nested octafile found for segment {}", segment);
          break;
        },
      }
    }
  }

  fn add_task_vars(&self, cmd: &FindResult, vars: Vars) -> Vars {
    // Add variables from current task
    match cmd.task.vars.clone() {
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
    }
  }

  fn add_task_envs(&self, cmd: &FindResult, envs: Envs) -> Envs {
    // Add environments from current task
    match cmd.task.env.clone() {
      Some(task_envs) => {
        let mut new_envs = Envs::new();
        new_envs.set_parent(Some(envs));
        new_envs.set_value(task_envs);

        new_envs
      },
      None => {
        let mut new_envs = Envs::new();
        new_envs.set_parent(Some(envs));

        new_envs
      },
    }
  }

  fn collect_envs(&self, cmd: &FindResult) -> Envs {
    let mut envs = self.initialize_global_envs(cmd);
    self.process_hierarchy_envs(cmd, &mut envs);
    self.add_task_envs(cmd, envs)
  }

  fn collect_vars(&self, cmd: &FindResult) -> Vars {
    let mut vars = self.initialize_global_vars(cmd);
    self.process_hierarchy_vars(cmd, &mut vars);
    self.add_task_vars(cmd, vars)
  }

  async fn add_command_to_dag(
    &self,
    dag: &mut DAG<TaskNode>,
    cmd: FindResult,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<Arc<TaskNode>> {
    // Check for complex command
    let nested_dag = match &cmd.task.cmds {
      Some(cmds) => self.build_nested_dag(&cmd, cmds, cancel_token.clone()).await?,
      None => None,
    };

    if let Some(nested_dag) = &nested_dag {
      self.validate_dag(nested_dag, &cmd.name)?;
    }

    let task = Arc::new(self.create_task_from_command(&cmd, nested_dag, None, None, None));

    let arc_task = Arc::clone(&task);
    dag.add_node(arc_task.clone());
    self
      .process_dependencies(dag, cmd.octafile, task, &cmd.task, cmd.name, cancel_token, &None)
      .await?;

    Ok(arc_task)
  }

  fn parse_multi_cmd<'a>(
    &'a self,
    cmd: &'a FindResult,
    cancel_token: CancellationToken,
  ) -> Pin<Box<dyn Future<Output = ExecutorResult<Option<DagNode>>> + 'a>> {
    Box::pin(async move {
      match &cmd.task.cmds {
        Some(cmds) => self.build_nested_dag(cmd, cmds, cancel_token).await,
        None => Ok(None),
      }
    })
  }

  async fn build_nested_dag(
    &self,
    cmd: &FindResult,
    cmds: &Vec<Cmds>,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<Option<DagNode>> {
    let mut nest_dag = DAG::new();
    let mut prev_node = None;
    // Index we use for simple command for set different name
    let mut index = 0;

    for command in cmds {
      match command {
        Cmds::Simple(_s) => {
          self.handle_simple_command(&mut nest_dag, cmd, command, index, &mut prev_node)?;

          index += 1;
        },
        Cmds::Complex(c) => {
          self
            .handle_complex_command(&mut nest_dag, cmd, c, &mut prev_node, cancel_token.clone())
            .await?;
        },
      }
    }

    Ok(Some(nest_dag))
  }

  fn handle_simple_command(
    &self,
    dag: &mut DagNode,
    cmd: &FindResult,
    command: &Cmds,
    index: usize,
    prev_node: &mut Option<Arc<TaskNode>>,
  ) -> ExecutorResult<()> {
    let cmd = FindResult {
      name: format!("{}_{}", cmd.name.clone(), index),
      octafile: cmd.octafile.clone(),
      task: Task {
        cmd: Some(command.clone()),
        deps: None,
        ..cmd.task.clone()
      },
    };

    let nested_task = Arc::new(self.create_task_from_command(&cmd, None, None, None, Some(CmdType::Internal)));

    dag.add_node(nested_task.clone());

    let should_add_dependency = cmd
      .task
      .execute_mode
      .as_ref()
      .map_or(true, |mode| mode == &ExecuteMode::Sequentially);

    if should_add_dependency {
      if let Some(prev) = &prev_node {
        dag.add_dependency(prev, &nested_task)?;
      }
    }

    *prev_node = Some(nested_task);

    Ok(())
  }

  async fn handle_complex_command(
    &self,
    nest_dag: &mut DagNode,
    cmd: &FindResult,
    complex: &ComplexCmd,
    prev_node: &mut Option<Arc<TaskNode>>,
    cancel_token: CancellationToken,
  ) -> ExecutorResult<()> {
    let cmds = self.finder.find_by_path(Arc::clone(&cmd.octafile), &complex.task);
    if cmds.is_empty() {
      return Err(ExecutorError::CommandNotFound(complex.task.clone()));
    }

    let should_add_dependency = cmd
      .task
      .execute_mode
      .as_ref()
      .map_or(true, |mode| mode == &ExecuteMode::Sequentially);

    for mut nested_cmd in cmds {
      if let Some(silent) = complex.silent {
        nested_cmd.task.silent = Some(silent);
      }

      let nested_dag = self.parse_multi_cmd(&nested_cmd, cancel_token.clone()).await?;

      if let Some(nested_dag) = &nested_dag {
        self.validate_dag(nested_dag, &cmd.name)?;
      }

      let nested_task = Arc::new(self.create_task_from_command(
        &nested_cmd,
        nested_dag,
        complex.vars.clone(),
        complex.envs.clone(),
        None,
      ));

      nest_dag.add_node(Arc::clone(&nested_task));

      self
        .process_dependencies(
          nest_dag,
          nested_cmd.octafile,
          nested_task.clone(),
          &nested_cmd.task,
          nested_cmd.name,
          cancel_token.clone(),
          prev_node,
        )
        .await?;

      if should_add_dependency {
        if let Some(prev) = prev_node {
          nest_dag.add_dependency(prev, &nested_task)?;
        }
      }

      *prev_node = Some(nested_task);
    }

    Ok(())
  }

  /// Creates a task from a command with proper variable handling
  fn create_task_from_command(
    &self,
    cmd: &FindResult,
    dag: Option<DagNode>,
    execute_vars: Option<octa_octafile::Vars>,
    execute_envs: Option<octa_octafile::Envs>,
    cmd_type: Option<CmdType>,
  ) -> TaskNode {
    let task = cmd.task.clone();

    let mut vars = self.collect_vars(cmd);
    vars.extend_with(&execute_vars);

    let mut envs = self.collect_envs(cmd);

    if let Some(env) = execute_envs {
      envs.extend(env.clone());
    }

    // Get task directory with fallback to taskfile directory
    let work_dir = task.dir.unwrap_or(cmd.octafile.dir.clone());

    let command = task.cmd.map(|cmd| cmd.to_string());

    let task_config = TaskConfig::builder()
      .id(cmd.name.clone())
      .name(cmd.name.clone())
      .dir(work_dir)
      .vars(vars)
      .envs(envs)
      .tpl(task.tpl)
      .sources(task.sources)
      .dag(dag)
      .silent(task.silent)
      .source_strategy(task.source_strategy)
      .ignore_errors(task.ignore_error)
      .run_mode(task.run)
      .cmd_type(cmd_type)
      .cmd(command);

    TaskNode::new(task_config.build().unwrap())
  }

  #[allow(clippy::too_many_arguments)]
  fn process_dependencies<'a>(
    &'a self,
    dag: &'a mut DagNode,
    octafile: Arc<Octafile>,
    parent: Arc<TaskNode>,
    task: &'a Task,
    curr_path: String,
    cancel_token: CancellationToken,
    prev_node: &'a Option<Arc<TaskNode>>,
  ) -> Pin<Box<dyn Future<Output = ExecutorResult<()>> + 'a>> {
    Box::pin(async move {
      let Some(deps) = &task.deps else { return Ok(()) };
      if deps.is_empty() {
        return Ok(());
      };

      for dep_info in self
        .collect_dependencies(octafile, deps, &curr_path, cancel_token.clone())
        .await
      {
        self
          .add_dependency(dag, dep_info, &parent, cancel_token.clone(), prev_node)
          .await?;
      }

      Ok(())
    })
  }

  // Similarly, update collect_dependencies
  fn collect_dependencies<'a>(
    &'a self,
    octafile: Arc<Octafile>,
    deps: &'a Vec<Deps>,
    curr_path: &'a str,
    cancel_token: CancellationToken,
  ) -> Pin<Box<dyn Future<Output = Vec<DependencyInfo>> + 'a>> {
    Box::pin(async move {
      iter(deps)
        .flat_map(|dep| {
          let octafile = octafile.clone();
          let finder = &self.finder;
          stream! {
            match dep {
              Deps::Simple(dep) => {
                for cmd in finder.find_by_path(octafile.clone(), dep) {
                  yield cmd;
                }
              },
              Deps::Complex(c) => {
                let result = finder.find_by_path(octafile.clone(), &c.task);

                for mut res in result {
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

                  yield res;
                }
              },
            }
          }
        })
        .then(|cmd| {
          let cancel_token = cancel_token.clone();
          let curr_path = curr_path.to_string();
          async move {
            let cmd_name = self.join_path(&curr_path, &cmd.name);
            let nested_dag = self.parse_multi_cmd(&cmd, cancel_token.clone()).await?;

            if let Some(nested_dag) = &nested_dag {
              self.validate_dag(nested_dag, &cmd.name)?;
            }

            let task = Arc::new(self.create_task_from_command(&cmd, nested_dag, None, None, None));

            Ok::<_, ExecutorError>(DependencyInfo {
              task,
              octafile: cmd.octafile,
              original_task: cmd.task,
              path: cmd_name,
            })
          }
        })
        .filter_map(|result| async move {
          match result {
            Ok(info) => Some(info),
            Err(e) => {
              tracing::error!("Error collecting dependency: {}", e);
              None
            },
          }
        })
        .collect()
        .await
    })
  }

  fn add_dependency<'a>(
    &'a self,
    dag: &'a mut DagNode,
    dep_info: DependencyInfo,
    parent: &'a Arc<TaskNode>,
    cancel_token: CancellationToken,
    prev_node: &'a Option<Arc<TaskNode>>,
  ) -> Pin<Box<dyn Future<Output = ExecutorResult<()>> + 'a>> {
    Box::pin(async move {
      dag.add_node(Arc::clone(&dep_info.task));
      dag.add_dependency(&dep_info.task, parent)?;

      // For complex command add dependency for previos node
      if let Some(prev) = &prev_node {
        dag.add_dependency(prev, &dep_info.task)?;
      }

      self
        .process_dependencies(
          dag,
          dep_info.octafile,
          Arc::clone(&dep_info.task),
          &dep_info.original_task,
          dep_info.path,
          cancel_token,
          prev_node,
        )
        .await
    })
  }

  fn join_path(&self, current: &str, segment: &str) -> String {
    if current.is_empty() {
      segment.to_string()
    } else {
      format!("{}:{}", current, segment)
    }
  }

  fn validate_dag(&self, dag: &DagNode, command: &str) -> ExecutorResult<()> {
    if dag.node_count() == 0 {
      return Err(ExecutorError::TaskNotFound(command.to_string()));
    }

    if dag.has_cycle()? {
      return Err(ExecutorError::CycleDetected);
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use octa_octafile::Octafile;
  use tempfile::TempDir;

  fn create_test_task() -> Task {
    Task {
      cmd: Some(Cmds::Simple("echo test".to_string())),
      ..Task::default()
    }
  }

  async fn setup_test_octafiles(temp_dir: &TempDir) -> ExecutorResult<Arc<Octafile>> {
    // Create root octafile content
    let root_content = r#"
      version: 1
      vars:
        ROOT_VAR: "root_value"
      includes:
        nested:
          octafile: nested/Octafile.yml
      tasks:
        root_task:
          cmd: echo "root"
    "#;

    // Create nested octafile content
    let nested_content = r#"
      version: 1
      vars:
        NESTED_VAR: "nested_value"
      includes:
        deep:
          octafile: deep/Octafile.yml
      tasks:
        nested_task:
          cmd: echo "nested"
    "#;

    // Create deep octafile content
    let deep_content = r#"
      version: 1
      vars:
        DEEP_VAR: "deep_value"
      tasks:
        deep_task:
          cmd: echo "deep"
    "#;

    // Create directory structure and write files
    let root_path = temp_dir.path().join("Octafile.yml");
    let nested_dir = temp_dir.path().join("nested");
    let deep_dir = nested_dir.join("deep");
    std::fs::create_dir(&nested_dir)?;
    std::fs::create_dir(&deep_dir)?;
    let nested_path = nested_dir.join("Octafile.yml");
    let deep_path = deep_dir.join("Octafile.yml");

    std::fs::write(&root_path, root_content)?;
    std::fs::write(&nested_path, nested_content)?;
    std::fs::write(&deep_path, deep_content)?;

    // Load the root octafile
    Ok(Octafile::load(Some(root_path), false)?)
  }

  #[tokio::test]
  async fn test_process_hierarchy_vars() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let root_octafile = setup_test_octafiles(&temp_dir).await?;

    let nested_octafile = root_octafile.get_included("nested")?.unwrap();
    let deep_octafile = nested_octafile.get_included("deep")?.unwrap();

    let cmd = FindResult {
      name: "test_cmd".to_string(),
      octafile: deep_octafile,
      task: create_test_task(),
    };

    let builder = TaskGraphBuilder::new()?;
    let mut vars = Vars::new();
    builder.process_hierarchy_vars(&cmd, &mut vars);

    vars.expand(false).await?;

    // Updated assertions for Tera values
    assert_eq!(vars.get("NESTED_VAR").and_then(|v| v.as_str()), Some("nested_value"));
    assert_eq!(vars.get("DEEP_VAR").and_then(|v| v.as_str()), Some("deep_value"));
    assert!(vars.get("TASKFILE_DIR").is_some());

    Ok(())
  }
}
