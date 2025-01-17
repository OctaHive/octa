/// Module for building and managing task execution graphs
pub mod envs;
pub mod error;
pub mod executor;
mod function;
mod hash_source;
pub mod summary;
pub mod task;
mod timestamp_source;
pub mod vars;

use std::{collections::HashMap, env, path::PathBuf, sync::Arc};

use envs::Envs;
use tracing::{debug, info};
use uuid::Uuid;

use error::{ExecutorError, ExecutorResult};
pub use executor::Executor;
use octa_dag::DAG;
use octa_finder::{FindResult, OctaFinder};
use octa_octafile::{AllowedRun, Cmds, Deps, ExecuteMode, Octafile, Task};
pub use task::TaskNode;
use task::{CmdType, TaskConfig};
use vars::Vars;

// Type aliases for better readability
type DagNode = DAG<TaskNode>;
type ArcNode = Arc<TaskNode>;

pub struct TaskGraphBuilder {
  finder: Arc<OctaFinder>,   // Finder for search task in octafile
  dir: PathBuf,              // Current user directory
  command_args: Vec<String>, // Additional task arguments from cli
  os_arch: String,           // Operating system architecture
  os_type: String,           // Operating system type
}

impl TaskGraphBuilder {
  /// Creates a new TaskGraphBuilder instance
  pub fn new() -> ExecutorResult<Self> {
    let current_dir = env::current_dir()?;
    let os_type = whoami::platform().to_string().replace(" ", "").to_lowercase();
    let os_arch = whoami::arch().to_string().replace(" ", "").to_lowercase();

    Ok(Self {
      finder: Arc::new(OctaFinder::new()),
      dir: current_dir,
      command_args: vec![],
      os_arch,
      os_type,
    })
  }

  /// Builds a DAG (Directed Acyclic Graph) of tasks from the given Octafile
  ///
  /// # Arguments
  /// * `octafile` - Reference to the Octafile containing task definitions
  /// * `command` - Command to execute
  /// * `run_parallel` - Whether tasks can run in parallel
  /// * `command_args` - Additional command line arguments
  pub async fn build(
    mut self,
    octafile: Arc<Octafile>,
    command: &str,
    run_parallel: bool,
    command_args: Vec<String>,
  ) -> ExecutorResult<DAG<TaskNode>> {
    info!(
      "Building DAG for command {} with provided args {:?}",
      command, command_args
    );

    self.command_args = command_args;
    let mut dag = DAG::new();

    let mut commands = self.find_and_filter_commands(&octafile, command)?;
    commands = self.filter_command_by_platform(commands);
    commands = self.filter_internal_task(commands);

    if commands.is_empty() {
      return Err(ExecutorError::CommandNotFound(command.to_string()));
    }

    for cmd in commands {
      let deps = self.process_dependencies(&mut dag, &cmd, vec![])?;

      self.process_command(
        &mut dag,
        cmd.name.clone(),
        &cmd,
        deps,
        &mut None,
        None,
        None,
        Some(run_parallel),
      )?;
    }

    self.validate_dag(&dag, command)?;

    Ok(dag)
  }

  #[allow(clippy::too_many_arguments)]
  fn process_command(
    &self,
    dag: &mut DagNode,
    dep_name: String,
    command: &FindResult,
    parent: Option<ArcNode>,
    prev: &mut Option<ArcNode>,
    execute_vars: Option<octa_octafile::Vars>,
    execute_envs: Option<octa_octafile::Envs>,
    run_parallel: Option<bool>,
  ) -> ExecutorResult<usize> {
    let run_parallel = run_parallel.unwrap_or(matches!(&command.task.execute_mode, Some(ExecuteMode::Parallel)));

    match &command.task.cmds {
      Some(cmds) => {
        let mut index = 0;

        for cmd in cmds {
          match cmd {
            Cmds::Simple(s) => {
              let simple = self.create_simple_command(command, s);

              // Fix command name
              let task = self.create_task_node(
                dag,
                dep_name.clone(),
                &simple,
                execute_vars.clone(),
                execute_envs.clone(),
              )?;

              // Если есть группирующая задача привязывает созданную к ней
              // Эта задача запуститься только когда выполнится parent
              if let Some(parent) = parent.clone() {
                dag.add_dependency(&parent, &task)?;
              }

              if !run_parallel {
                if let Some(prev) = prev {
                  dag.add_dependency(prev, &task)?;
                }

                *prev = Some(task)
              }

              index += 1;
            },
            Cmds::Complex(complex) => {
              let mut cmds = self.find_and_filter_commands(&command.octafile, &complex.task)?;
              cmds = self.filter_command_by_platform(cmds);

              if cmds.is_empty() {
                continue;
              }

              for cmd in cmds {
                let mut deps =
                  self.process_dependencies(dag, &cmd, vec![prev.as_ref().map(Arc::clone), parent.clone()])?;

                // Если нам пришла зависимая задача, то привязываем наши deps
                // к ней, чтобы выстроить цепочку выполнения
                if let Some(deps) = &deps {
                  if let Some(parent) = &parent {
                    dag.add_dependency(parent, deps)?;
                  }
                } else {
                  deps = parent.clone();
                }

                let cnt = self.process_command(
                  dag,
                  dep_name.clone(),
                  &cmd,
                  deps,
                  prev,
                  complex.vars.clone(),
                  complex.envs.clone(),
                  None,
                )?;

                index += cnt;
              }
            },
          }
        }

        Ok(index)
      },
      None => {
        let task = self.create_task_node(dag, dep_name, command, execute_vars.clone(), execute_envs.clone())?;

        // Если есть задача от deps, то привязываем к ней
        if let Some(parent) = parent.clone() {
          dag.add_dependency(&parent, &task)?;
        }

        if !run_parallel {
          if let Some(prev) = prev {
            dag.add_dependency(prev, &task)?;
          }

          *prev = Some(task)
        }

        Ok(1)
      },
    }
  }

  #[allow(clippy::too_many_arguments)]
  fn process_deps_command(
    &self,
    dag: &mut DagNode,
    dep_name: String,
    command: &FindResult,
    execute_vars: Option<octa_octafile::Vars>,
    execute_envs: Option<octa_octafile::Envs>,
    prev: &mut Option<Arc<TaskNode>>,
    group: ArcNode,
    parents: Vec<Option<ArcNode>>,
  ) -> ExecutorResult<()> {
    match &command.task.cmds {
      Some(cmds) => {
        // Зависимости по умолчанию выполняются паралелльно
        let run_parallel = matches!(&command.task.execute_mode, Some(ExecuteMode::Parallel));

        for cmd in cmds {
          match cmd {
            Cmds::Simple(s) => {
              let simple = self.create_simple_command(command, s);

              let task = self.create_task_node(
                dag,
                dep_name.clone(),
                &simple,
                execute_vars.clone(),
                execute_envs.clone(),
              )?;

              let deps = self.process_dependencies(dag, &simple, vec![prev.as_ref().map(Arc::clone)])?;
              if let Some(deps) = &deps {
                dag.add_dependency(deps, &task)?;
              }

              // Добавляем связь с группирующей задачей
              dag.add_dependency(&task, &group)?;

              for parent in parents.iter().flatten() {
                dag.add_dependency(parent, &task)?;
              }

              // Если задачи должны запускаться не параллельно,
              // то добавляем зависимость между задачами
              if !run_parallel {
                if let Some(prev) = prev {
                  dag.add_dependency(prev, &task)?;
                }

                *prev = Some(task)
              }
            },
            Cmds::Complex(complex) => {
              let mut cmds = self.find_and_filter_commands(&command.octafile, &complex.task)?;
              cmds = self.filter_command_by_platform(cmds);

              if cmds.is_empty() {
                continue;
              }

              for cmd in cmds {
                let task =
                  self.create_task_node(dag, dep_name.clone(), &cmd, complex.vars.clone(), complex.envs.clone())?;

                let deps = self.process_dependencies(dag, &cmd, vec![prev.as_ref().map(Arc::clone)])?;
                if let Some(deps) = &deps {
                  dag.add_dependency(deps, &task)?;
                }

                // Добавляем связь с группирующей задачей
                dag.add_dependency(&task, &group)?;

                for parent in parents.iter().flatten() {
                  dag.add_dependency(parent, &task)?;
                }

                // Если задачи должны запускаться не параллельно,
                // то добавляем зависимость между задачами
                if !run_parallel {
                  if let Some(prev) = prev {
                    dag.add_dependency(prev, &task)?;
                  }

                  *prev = Some(task)
                }
              }
            },
          }
        }
      },
      None => {
        // Зависимости по умолчанию выполняются паралелльно
        let run_parallel = !matches!(&command.task.execute_mode, Some(ExecuteMode::Sequentially));

        let task = self.create_task_node(dag, dep_name, command, execute_vars, execute_envs)?;

        let deps = self.process_dependencies(dag, command, vec![prev.as_ref().map(Arc::clone)])?;
        if let Some(deps) = &deps {
          dag.add_dependency(deps, &task)?;
        }

        // Добавляем связь с группирующей задачей
        dag.add_dependency(&task, &group)?;

        for parent in parents.iter().flatten() {
          dag.add_dependency(parent, &task)?;
        }

        // Если задачи должны запускаться не параллельно,
        // то добавляем зависимость между задачами
        if !run_parallel {
          if let Some(prev) = prev {
            dag.add_dependency(prev, &task)?;
          }

          *prev = Some(task)
        }
      },
    }

    Ok(())
  }

  /// Creates a task node with the given configuration
  fn create_task_node(
    &self,
    dag: &mut DagNode,
    dep_name: String,
    cmd: &FindResult,
    execute_vars: Option<octa_octafile::Vars>,
    execute_envs: Option<octa_octafile::Envs>,
  ) -> ExecutorResult<ArcNode> {
    let envs = self.collect_envs(cmd, execute_envs);
    let vars = self.collect_vars(cmd, execute_vars);

    let task_config = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(cmd.name.clone())
      .dep_name(dep_name)
      .dir(cmd.task.dir.clone().unwrap_or(cmd.octafile.dir.clone()))
      .vars(vars)
      .envs(envs)
      .preconditions(cmd.task.preconditions.clone())
      .sources(cmd.task.sources.clone())
      .silent(cmd.task.silent)
      .source_strategy(cmd.task.source_strategy.clone())
      .ignore_errors(cmd.task.ignore_error)
      .run_mode(cmd.task.run.clone())
      .cmd(cmd.task.cmd.clone().map(|cmd| cmd.to_string()));

    let task = TaskNode::new(task_config.build().unwrap());
    let arc_task = Arc::new(task);

    // Добавить созданную ноду в граф
    dag.add_node(arc_task.clone());

    Ok(arc_task)
  }

  /// Process task dependencies and build the dependency graph
  ///
  /// # Arguments
  /// * `dag` - The DAG being built
  /// * `cmd` - The command containing dependencies
  /// * `parents` - Parent nodes in the graph
  ///
  /// # Returns
  /// Optional group node that contains all dependencies
  fn process_dependencies(
    &self,
    dag: &mut DagNode,
    cmd: &FindResult,
    parents: Vec<Option<ArcNode>>,
  ) -> ExecutorResult<Option<ArcNode>> {
    let Some(deps) = &cmd.task.deps else {
      return Ok(None);
    };

    // Собираем карту задач, чтобы понять встречается задача несколько раз или нет
    // Это необходимо чтобы правильно задать имя задачи для сохранения результатов
    // выполонения
    let deps_map = self.build_deps_frequency_map(deps);

    // Мы создаем группирующую ноду для всех зависимостей
    // чтобы комплексная задача могла завязаться на эту
    // группирующую задачу. Это внутренная задача и она не
    // видна в результатах выполнения плана задач.
    let group = self.create_group_node(
      dag,
      cmd.task.run.clone(),
      format!("Group deps task for command {}", cmd.name),
    )?;

    // По умолчанию все зависимости обрабатываются параллельно, но
    // если выбран режим последовательного запуска, то нам надо
    // связать задачи между собой
    let prev: &mut Option<Arc<TaskNode>> = &mut None;

    for dep in deps {
      match dep {
        Deps::Simple(dep_name) => {
          let mut commands = self.find_and_filter_commands(&cmd.octafile, dep_name)?;
          commands = self.filter_command_by_platform(commands);

          if commands.is_empty() {
            continue;
          }

          for cmd in commands {
            let task_name = self.generate_unique_task_name(dep_name, &deps_map);

            self.process_deps_command(dag, task_name, &cmd, None, None, prev, group.clone(), parents.clone())?;
          }
        },
        Deps::Complex(c) => {
          let mut commands = self.find_and_filter_commands(&cmd.octafile, &c.task)?;
          commands = self.filter_command_by_platform(commands);

          if commands.is_empty() {
            continue;
          }

          for cmd in commands {
            let task_name = self.generate_unique_task_name(&c.task, &deps_map);

            self.process_deps_command(
              dag,
              task_name,
              &cmd,
              c.vars.clone(),
              c.envs.clone(),
              prev,
              group.clone(),
              parents.clone(),
            )?;
          }
        },
      }
    }

    Ok(Some(group))
  }

  /// Generate a unique task name based on frequency map
  fn generate_unique_task_name(&self, task_name: &str, deps_map: &HashMap<&str, (usize, usize)>) -> String {
    if let Some(&(count, index)) = deps_map.get(task_name) {
      if count > 1 {
        format!("{}_{}", task_name, index + 1)
      } else {
        task_name.to_string()
      }
    } else {
      task_name.to_string()
    }
  }

  fn filter_internal_task(&self, tasks: Vec<FindResult>) -> Vec<FindResult> {
    tasks
      .into_iter()
      .filter(|t| !t.task.internal.unwrap_or(false))
      .collect()
  }

  /// Build a map tracking frequency of each dependency
  fn build_deps_frequency_map<'a>(&self, deps: &'a [Deps]) -> HashMap<&'a str, (usize, usize)> {
    let mut deps_map = HashMap::new();

    for dep in deps {
      match dep {
        Deps::Simple(name) => {
          deps_map
            .entry(name.as_str())
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, 0));
        },
        Deps::Complex(complex) => {
          deps_map
            .entry(complex.task.as_str())
            .and_modify(|(count, _)| *count += 1)
            .or_insert((1, 0));
        },
      }
    }

    deps_map
  }

  /// Find and filter commands by platform
  fn find_and_filter_commands(&self, octafile: &Arc<Octafile>, task_name: &str) -> ExecutorResult<Vec<FindResult>> {
    let cmds = self.finder.find_by_path(Arc::clone(octafile), task_name);

    if cmds.is_empty() {
      return Err(ExecutorError::CommandNotFound(task_name.to_string()));
    }

    Ok(cmds)
  }

  /// Create a simple command from a complex one
  fn create_simple_command(&self, command: &FindResult, cmd_str: &str) -> FindResult {
    FindResult {
      name: cmd_str.to_string(),
      octafile: command.octafile.clone(),
      task: Task {
        cmds: None,
        cmd: Some(Cmds::Simple(cmd_str.to_string())),
        deps: None,
        ..command.task.clone()
      },
    }
  }

  fn create_group_node(
    &self,
    dag: &mut DagNode,
    run: Option<AllowedRun>,
    name: String,
  ) -> ExecutorResult<Arc<TaskNode>> {
    let task_config = TaskConfig::builder()
      .id(Uuid::new_v4())
      .name(name.clone())
      .run_mode(run)
      .dep_name(name)
      .cmd_type(Some(CmdType::Internal));

    let task = TaskNode::new(task_config.build().unwrap());
    let arc_task = Arc::new(task);

    dag.add_node(arc_task.clone());

    Ok(arc_task)
  }

  /// Collects variables from global, hierarchy and task levels
  fn collect_vars(&self, cmd: &FindResult, execute_vars: Option<octa_octafile::Vars>) -> Vars {
    let mut vars = self.initialize_global_vars(cmd);
    let env_vars: HashMap<String, String> = env::vars().collect();

    self.process_hierarchy_vars(cmd, &mut vars);
    vars = self.add_task_vars(cmd, vars);
    if let Some(exec_vars) = execute_vars {
      vars.extend_with(&Some(exec_vars));
    }

    vars.extend_with(&env_vars);

    vars
  }

  fn initialize_global_vars(&self, cmd: &FindResult) -> Vars {
    let mut vars = Vars::new();
    let os_type = whoami::platform();
    let os_arch = whoami::arch();
    let root = cmd.octafile.root();

    vars.set_value(root.vars.clone());

    vars.insert("ROOT_DIR", &root.dir.display().to_string());
    vars.insert("OCTAFILE_DIR", &root.dir.display().to_string());
    vars.insert("USER_WORKING_DIR", &self.dir.display().to_string());
    vars.insert("COMMAND_ARGS", &self.command_args);
    vars.insert("OCTA_OS", &os_type.to_string());
    vars.insert("OCTA_ARCH", &os_arch.to_string());

    vars
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

  /// Collects environments from global, hierarchy and task levels
  fn collect_envs(&self, cmd: &FindResult, execute_envs: Option<octa_octafile::Envs>) -> Envs {
    let mut envs = self.initialize_global_envs(cmd);
    self.process_hierarchy_envs(cmd, &mut envs);
    envs = self.add_task_envs(cmd, envs);
    if let Some(exec_vars) = execute_envs {
      envs.extend(exec_vars.clone());
    }
    envs
  }

  fn initialize_global_envs(&self, cmd: &FindResult) -> Envs {
    let mut envs = Envs::new();
    let root = cmd.octafile.root();

    if let Some(env) = &root.env {
      envs.set_value(env.clone());
    }

    envs
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

  fn filter_command_by_platform(&self, commands: Vec<FindResult>) -> Vec<FindResult> {
    commands
      .into_iter()
      .filter(|cmd| {
        if let Some(platforms) = &cmd.task.platforms {
          return platforms.contains(&self.os_type)
            || platforms.contains(&self.os_arch)
            || platforms.contains(&format!("{}/{}", &self.os_type, &self.os_arch).to_string());
        }

        true
      })
      .collect()
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
  use std::fs;
  use tempfile::TempDir;

  fn create_test_task() -> Task {
    Task {
      cmd: Some(Cmds::Simple("echo test".to_string())),
      ..Task::default()
    }
  }

  #[tokio::test]
  async fn test_task_graph_builder_new() -> ExecutorResult<()> {
    let builder = TaskGraphBuilder::new()?;
    assert!(builder.command_args.is_empty());
    assert!(builder.dir.exists());
    Ok(())
  }

  #[tokio::test]
  async fn test_build_simple_task() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test:
          cmd: echo "test"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false)?;
    let builder = TaskGraphBuilder::new()?;
    let dag = builder.build(octafile, "test", true, vec![]).await?;

    assert_eq!(dag.node_count(), 1);
    assert!(!dag.has_cycle()?);
    let tasks: Vec<String> = dag.nodes().iter().map(|n| n.name.clone()).collect();

    assert!(tasks.contains(&"test".to_owned()));
    Ok(())
  }

  #[tokio::test]
  async fn test_build_with_dependencies() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        task1:
          cmd: echo "task1"
        task2:
          cmd: echo "task2"
          deps:
            - task1
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false)?;
    let builder = TaskGraphBuilder::new()?;
    let dag = builder.build(octafile, "task2", true, vec![]).await?;

    let id_to_name: HashMap<String, String> = dag
      .nodes()
      .iter()
      .map(|item| (item.name.clone(), item.id.clone()))
      .collect();

    assert_eq!(dag.node_count(), 3);
    assert!(!dag.has_cycle()?);
    let tasks: Vec<String> = dag.nodes().iter().map(|n| n.name.clone()).collect();
    assert!(tasks.contains(&"task1".to_owned()));
    assert!(tasks.contains(&"task2".to_owned()));

    assert!(dag.edges().contains_key(&id_to_name["task1"]));

    Ok(())
  }

  #[tokio::test]
  async fn test_command_not_found() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test:
          cmd: echo "test"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false)?;
    let builder = TaskGraphBuilder::new()?;
    let result = builder.build(octafile, "nonexistent", true, vec![]).await;

    assert!(matches!(result, Err(ExecutorError::CommandNotFound(_))));
    Ok(())
  }

  #[tokio::test]
  async fn test_platform_specific_tasks() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test_macos:
          cmd: echo "test"
          platforms:
            - macos
        test_linux:
          cmd: echo "test"
          platforms:
            - linux
        test_windows:
          cmd: echo "test"
          platforms:
            - windows
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false)?;
    let builder = TaskGraphBuilder::new()?;

    let dag = if cfg!(target_os = "linux") {
      builder.build(octafile, "test_linux", true, vec![]).await?
    } else if cfg!(target_os = "windows") {
      builder.build(octafile, "test_windows", true, vec![]).await?
    } else {
      builder.build(octafile, "test_macos", true, vec![]).await?
    };

    // The number of nodes will depend on the current platform
    assert!(!dag.has_cycle()?);
    Ok(())
  }

  #[tokio::test]
  async fn test_command_with_args() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      tasks:
        test:
          cmd: echo "{{ COMMAND_ARGS }}"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false)?;
    let builder = TaskGraphBuilder::new()?;
    let args = vec!["arg1".to_string(), "arg2".to_string()];
    let dag = builder.build(octafile, "test", true, args).await?;

    assert_eq!(dag.node_count(), 1);
    Ok(())
  }

  #[tokio::test]
  async fn test_variable_inheritance() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      vars:
        GLOBAL: "global"
      tasks:
        test:
          vars:
            LOCAL: "local"
          cmd: echo "{{ GLOBAL }} {{ LOCAL }}"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false)?;
    let builder = TaskGraphBuilder::new()?;
    let dag = builder.build(octafile, "test", true, vec![]).await?;

    assert_eq!(dag.node_count(), 1);
    Ok(())
  }

  #[tokio::test]
  async fn test_environment_inheritance() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let content = r#"
      version: 1
      env:
        GLOBAL_ENV: "global"
      tasks:
        test:
          env:
            LOCAL_ENV: "local"
          cmd: echo "test"
    "#;
    let octafile_path = temp_dir.path().join("Octafile.yml");
    fs::write(&octafile_path, content)?;

    let octafile = Octafile::load(Some(octafile_path), false)?;
    let builder = TaskGraphBuilder::new()?;
    let dag = builder.build(octafile, "test", true, vec![]).await?;

    assert_eq!(dag.node_count(), 1);
    Ok(())
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

  #[tokio::test]
  async fn test_nested_includes() -> ExecutorResult<()> {
    let temp_dir = TempDir::new().unwrap();
    let root_octafile = setup_test_octafiles(&temp_dir).await?;

    let builder = TaskGraphBuilder::new()?;
    let dag = builder.build(root_octafile, "**:deep_task", true, vec![]).await?;

    assert!(dag.node_count() > 0);
    assert!(!dag.has_cycle()?);
    Ok(())
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
}
