use std::{
  collections::HashMap,
  env,
  fmt::{self, Display, Formatter},
  hash::{Hash, Hasher},
  path::PathBuf,
  sync::Arc,
  time::Duration,
};

use async_trait::async_trait;
use serde::{Serialize, Serializer};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tera::{Context, Tera};
use tokio::{select, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, enabled, error, info, Level};

use octa_dag::Identifiable;
use octa_octafile::Task;

use crate::{
  error::{ExecutorError, ExecutorResult},
  vars::Vars,
};

const USER_WORKING_DIR: &str = "USER_WORKING_DIR";
const TASKFILE_DIR: &str = "TASKFILE_DIR";
const ROOT_DIR: &str = "ROOT_DIR";

#[async_trait]
pub trait Executable {
  async fn execute(&self) -> ExecutorResult<TaskResult>;
  async fn set_result(&self, task_name: String, res: TaskResult);
}

#[derive(Debug, Clone)]
pub enum TaskResult {
  Single(String),
  Group(Vec<String>),
}

impl Serialize for TaskResult {
  fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
  where
    S: Serializer,
  {
    match self {
      TaskResult::Single(result) => result.serialize(serializer),
      TaskResult::Group(results) => results.serialize(serializer),
    }
  }
}

impl Display for TaskResult {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    match self {
      TaskResult::Single(value) => write!(f, "{}", value),
      TaskResult::Group(values) => {
        write!(f, "[")?;
        let mut iter = values.iter();
        if let Some(first) = iter.next() {
          write!(f, "{}", first)?;
        }
        for item in iter {
          write!(f, ", {}", item)?;
        }
        write!(f, "]")
      },
    }
  }
}

/// Represents a single executable task with its configuration and state
#[derive(Debug, Clone)]
pub struct TaskNode {
  pub name: String,                                      // Task name
  pub cmd: Option<String>,                               // Command to execute
  pub dir: PathBuf,                                      // Working directory
  pub ignore_errors: bool,                               // Whether to continue on error
  pub vars: Vars,                                        // Task variables
  pub deps_res: Arc<Mutex<HashMap<String, TaskResult>>>, // Dependencies results
  cancel_token: CancellationToken,
}

// Implement equality based on task ID
impl Eq for TaskNode {}

impl PartialEq for TaskNode {
  fn eq(&self, other: &Self) -> bool {
    self.name == other.name
  }
}

// Implement hashing based on task name
impl Hash for TaskNode {
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.name.hash(state);
  }
}

impl TaskNode {
  pub fn new(name: String, task: Task, dir: PathBuf, vars: Vars, cancel_token: CancellationToken) -> Self {
    Self {
      name,
      cmd: task.cmd.clone(),
      ignore_errors: task.ignore_error.unwrap_or_default(),
      vars,
      dir,
      deps_res: Arc::new(Mutex::new(HashMap::default())),
      cancel_token,
    }
  }

  /// Executes a shell command and returns its output
  async fn execute_command(&self, cmd: &str) -> ExecutorResult<String> {
    let rendered_cmd = self.render_template(cmd).await?;

    debug!("Execute command: {}", rendered_cmd);
    let dir = self.interpolate_dir(self.dir.clone())?;

    #[cfg(windows)]
    let mut command = {
      const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
      const CREATE_NO_WINDOW: u32 = 0x08000000;

      let mut cmd = tokio::process::Command::new("cmd");
      cmd
        .current_dir(dir)
        .args(["/C", &rendered_cmd])
        .kill_on_drop(true)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    };

    #[cfg(not(windows))]
    let mut command = {
      let mut cmd = tokio::process::Command::new("sh");
      cmd
        .current_dir(&dir)
        .arg("-c")
        .arg(&rendered_cmd)
        .kill_on_drop(true)
        .process_group(0);
      cmd
    };

    let mut child = command.spawn()?;

    let output = select! {
        _ = child.wait() => {
          child.wait_with_output().await?
        },
        _ = self.cancel_token.cancelled() => {
          info!("Shutting down task {}", self.name);

          // Kill the entire process group
          #[cfg(windows)]
          {
            use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
            if let Some(pid) = child.id() {
              unsafe {
                let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if handle != 0 {
                  TerminateProcess(handle, 1);
                }
              }
            }
          }

          #[cfg(unix)]
          {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;
            if let Some(pid) = child.id() {
              info!("Kill task with pid {}", pid);
              let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGTERM);
            }
          }

          // Give the process a moment to cleanup
          tokio::time::sleep(Duration::from_millis(100)).await;

          // Force kill if still running
          info!("Kill child command");
          let _ = child.kill().await?;
          child.wait_with_output().await?;

          return Err(ExecutorError::TaskCancelled(self.name.clone()))
        }
    };

    if !output.status.success() && !self.cancel_token.is_cancelled() {
      let error = String::from_utf8_lossy(&output.stderr);
      return Err(ExecutorError::CommandFailed(format!(
        "Task {} failed: {}",
        self.name, error
      )));
    }

    let output_res = String::from_utf8_lossy(&output.stdout);
    let stderr_res = String::from_utf8_lossy(&output.stderr);

    if !output_res.is_empty() {
      info!("{}", output_res.trim());
    }

    if !stderr_res.is_empty() {
      info!("{}", stderr_res.trim());
    }

    Ok(output_res.trim().into())
  }

  /// Renders the command template with variables and dependency results
  async fn render_template(&self, template: &str) -> ExecutorResult<String> {
    let mut tera = Tera::default();
    let template_name = format!("task_{}", self.name);

    tera.add_raw_template(&template_name, template).map_err(|e| {
      ExecutorError::TemplateParseFailed(format!("Failed to parse template for task {}: {}", self.name, e))
    })?;

    let mut vars = self.vars.clone();
    vars.interpolate().await?;
    let mut context: Context = vars.into();
    // Add dependency results to template context
    let deps_res = self.deps_res.lock().await;
    context.insert("deps_result", &*deps_res);

    tera.render(&template_name, &context).map_err(|e| {
      ExecutorError::TemplateRenderError(format!("Failed to render template for task {}: {}", self.name, e))
    })
  }

  fn interpolate_dir(&self, dir: PathBuf) -> ExecutorResult<PathBuf> {
    let mut tera = Tera::default();
    let current_dir = env::current_dir()?;
    let mut context = Context::new();
    let dir_str = dir.to_string_lossy();

    if !dir_str.contains("{{") || !dir_str.contains("}}") {
      debug!("Using direct directory path: {}", dir_str);

      Ok(dir.clone())
    } else {
      debug!("Interpolating directory path: {}", dir_str);

      context.insert(USER_WORKING_DIR, &current_dir);
      // context.insert(TASKFILE_DIR, &self._dir);
      // context.insert(ROOT_DIR, &self.taskfile.root()._dir);

      let rendered = tera
        .render_str(&dir_str, &context)
        .map_err(|e| ExecutorError::ValueInterpolateError(dir_str.to_string(), e.to_string()))?;

      debug!("Interpolated path: {}", rendered);
      Ok(PathBuf::from(rendered))
    }
  }
}

impl Identifiable for TaskNode {
  fn id(&self) -> String {
    self.name.clone()
  }
}

#[async_trait]
impl Executable for TaskNode {
  /// Stores the result of a dependent task
  async fn set_result(&self, task_name: String, res: TaskResult) {
    let mut deps_res = self.deps_res.lock().await;

    deps_res.insert(task_name, res);
  }

  /// Executes the task and returns the result
  async fn execute(&self) -> ExecutorResult<TaskResult> {
    info!("Starting task {}", self.name);

    // Debug information about dependency results
    if enabled!(Level::DEBUG) {
      let deps = self.deps_res.lock().await;

      for (name, res) in &*deps {
        debug!("Dependency {} results: {}", name, res);
      }
    }

    match &self.cmd {
      Some(cmd) => match self.execute_command(cmd).await {
        Ok(res) => {
          info!("Completed task {}", self.name);

          Ok(TaskResult::Single(res))
        },
        Err(ExecutorError::TaskCancelled(_)) => Ok(TaskResult::Single(String::new())),
        Err(e) => {
          if self.ignore_errors {
            error!("Task {} failed but errors ignored. Error: {}", self.name, e);
            Ok(TaskResult::Single(String::new()))
          } else {
            Err(ExecutorError::TaskFailed(e.to_string()))
          }
        },
      },
      None => Ok(TaskResult::Single(String::new())),
    }
  }
}
