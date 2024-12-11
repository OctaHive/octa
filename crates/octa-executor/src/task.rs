use std::{
  collections::HashMap,
  hash::{Hash, Hasher},
  path::PathBuf,
  process::Stdio,
  sync::Arc,
  time::Duration,
};

use async_trait::async_trait;
use indexmap::IndexMap;
use sled::Db;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use tera::{Context, Tera};
use tokio::{select, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing::{debug, enabled, error, info, Level};

use octa_dag::{Identifiable, DAG};
use octa_octafile::{AllowedRun, SourceStrategies};

use crate::{
  error::{ExecutorError, ExecutorResult},
  executor::ExecutorConfig,
  hash_source::HashSource,
  timestamp_source::TimestampSource,
  vars::Vars,
  Executor,
};

pub trait TaskItem {
  fn run_mode(&self) -> RunMode;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceMethod {
  Timestamp,
  Hash,
}

impl From<SourceStrategies> for SourceMethod {
  fn from(value: SourceStrategies) -> Self {
    match value {
      SourceStrategies::Timestamp => SourceMethod::Timestamp,
      SourceStrategies::Hash => SourceMethod::Hash,
    }
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunMode {
  Always,
  Once,
  Changed,
}

impl From<AllowedRun> for RunMode {
  fn from(value: AllowedRun) -> Self {
    match value {
      AllowedRun::Once => RunMode::Once,
      AllowedRun::Always => RunMode::Always,
      AllowedRun::Changed => RunMode::Changed,
    }
  }
}

#[async_trait]
pub trait Executable<T> {
  async fn execute(
    &self,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    fingerprint: Arc<Db>,
  ) -> ExecutorResult<String>;
  async fn set_result(&self, task_name: String, res: String);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdType {
  Normal,
  Internal,
}

/// Represents a single executable task with its configuration and state
#[derive(Debug, Clone)]
pub struct TaskNode {
  pub name: String,                                  // Task name
  pub cmd: Option<String>,                           // Command to execute
  pub tpl: Option<String>,                           // Template to render
  pub dir: PathBuf,                                  // Working directory
  pub ignore_errors: bool,                           // Whether to continue on error
  pub silent: bool,                                  // Should task print to stdout or stderr
  pub run_mode: RunMode,                             // Run mode
  pub vars: Vars,                                    // Task variables
  pub sources: Option<Vec<String>>,                  // Sources for fingerprinting
  pub source_strategy: SourceMethod,                 // Source validation strategy
  pub deps_res: Arc<Mutex<HashMap<String, String>>>, // Dependencies results
  pub dag: Option<DAG<TaskNode>>,                    // Execution plan for complex command
  cmd_type: CmdType,                                 // Type of task for internal use
  cancel_token: CancellationToken,                   // Using for canceling task execution
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

#[derive(Debug)]
pub struct CacheItem {
  result: String,
  vars: Vars,
}

impl CacheItem {
  pub fn new(result: String, vars: Vars) -> Self {
    Self { result, vars }
  }
}

impl TaskNode {
  pub fn new(
    name: String,
    dir: PathBuf,
    vars: Vars,
    cancel_token: CancellationToken,
    cmd: Option<String>,
    tpl: Option<String>,
    sources: Option<Vec<String>>,
    source_strategy: Option<SourceStrategies>,
    silent: Option<bool>,
    ignore_errors: Option<bool>,
    run_mode: Option<AllowedRun>,
    dag: Option<DAG<TaskNode>>,
    cmd_type: Option<CmdType>,
  ) -> Self {
    let run_mode = match run_mode {
      Some(run_mode) => run_mode.into(),
      None => RunMode::Always,
    };

    let source_strategy = match source_strategy {
      Some(source_strategy) => source_strategy.into(),
      None => SourceMethod::Hash,
    };

    Self {
      name,
      cmd,
      tpl,
      cancel_token,
      run_mode,
      sources,
      source_strategy,
      vars,
      dir,
      ignore_errors: ignore_errors.unwrap_or_default(),
      silent: silent.unwrap_or_default(),
      deps_res: Arc::new(Mutex::new(HashMap::default())),
      cmd_type: cmd_type.unwrap_or(CmdType::Normal),
      dag,
    }
  }

  /// Executes a shell command and returns its output
  async fn execute_command(&self, cmd: &str) -> ExecutorResult<String> {
    let rendered_cmd = self.render_template(cmd, None).await?;

    debug!("Execute command: {}", rendered_cmd);
    let dir = self.interpolate_dir(self.dir.clone()).await?;

    #[cfg(windows)]
    let mut command = {
      const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
      const CREATE_NO_WINDOW: u32 = 0x08000000;

      let mut cmd = tokio::process::Command::new("cmd");
      cmd
        .current_dir(dir)
        .args(["/C", &rendered_cmd])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    };

    #[cfg(not(windows))]
    let mut command = {
      let mut cmd = tokio::process::Command::new("sh");
      cmd
        .current_dir(&dir)
        .arg("-c")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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
        self.log_info(format!("Shutting down task {}", self.name));

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
            self.log_info(format!("Kill task with pid {}", pid));
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

    if !output_res.is_empty() && !self.silent {
      info!("{}", output_res.trim());
    }

    if !stderr_res.is_empty() && !self.silent {
      info!("{}", stderr_res.trim());
    }

    Ok(output_res.trim().into())
  }

  /// Renders the command template with variables and dependency results
  async fn render_template(
    &self,
    template: &str,
    cache: Option<Arc<Mutex<IndexMap<String, CacheItem>>>>,
  ) -> ExecutorResult<String> {
    // Interpolate vars
    let mut vars = self.vars.clone();
    vars.interpolate().await?;

    if self.run_mode != RunMode::Always {
      if let Some(ref cache) = cache {
        let cache_lock = cache.lock().await;
        if let Some(cached_result) = cache_lock.get(&self.name) {
          debug!("Cache hit for task: {}", self.name);
          return Ok(cached_result.result.to_string());
        }
      }
    }

    let mut tera = Tera::default();
    let template_name = format!("task_{}", self.name);

    tera.add_raw_template(&template_name, template).map_err(|e| {
      ExecutorError::TemplateParseFailed(format!("Failed to parse template for task {}: {}", self.name, e))
    })?;

    let mut context: Context = vars.clone().into();
    // Add dependency results to template context
    let deps_res = self.deps_res.lock().await;
    context.insert("deps_result", &*deps_res);

    let result = tera.render(&template_name, &context).map_err(|e| {
      ExecutorError::TemplateRenderError(format!("Failed to render template for task {}: {:?}", self.name, e))
    })?;

    if self.run_mode != RunMode::Always {
      if let Some(ref cache) = cache {
        let mut cache_lock = cache.lock().await;
        cache_lock.insert(self.name.clone(), CacheItem::new(result.clone(), vars));
        debug!("Cached result for task: {}", self.name);
      }
    }

    Ok(result)
  }

  async fn interpolate_dir(&self, dir: PathBuf) -> ExecutorResult<PathBuf> {
    let dir_str = dir.to_string_lossy();

    if !dir_str.contains("{{") || !dir_str.contains("}}") {
      debug!("Using direct directory path: {}", dir_str);

      Ok(dir)
    } else {
      debug!("Interpolating directory path: {}", dir_str);

      let mut tera = Tera::default();
      let mut vars = self.vars.clone();
      vars.interpolate().await?;

      let context: Context = vars.into();

      let rendered = tera
        .render_str(&dir_str, &context)
        .map_err(|e| ExecutorError::ValueInterpolateError(dir_str.to_string(), e.to_string()))?;

      debug!("Interpolated path: {}", rendered);

      Ok(PathBuf::from(rendered.trim_matches('"'))) // Remove extra quotes from result
    }
  }

  async fn execute_cmd(&self, cmd: String, cache: Arc<Mutex<IndexMap<String, CacheItem>>>) -> ExecutorResult<String> {
    let mut vars = self.vars.clone();
    vars.interpolate().await?;

    if self.run_mode != RunMode::Always {
      let cache_lock = cache.lock().await;
      if let Some(cached_result) = cache_lock.get(&self.name) {
        if cached_result.vars == vars {
          debug!("Cache hit for task: {}", self.name);
          return Ok(cached_result.result.clone());
        }
      }
    }

    let result = match self.execute_command(&cmd).await {
      Ok(res) => {
        if self.run_mode != RunMode::Always {
          let mut cache_lock = cache.lock().await;
          cache_lock.insert(self.name.clone(), CacheItem::new(res.clone(), vars.clone()));
          debug!("Cached result for task: {}", self.name);
        }

        Ok(res)
      },
      Err(ExecutorError::TaskCancelled(_)) => Ok("".to_owned()),
      Err(e) => {
        if self.ignore_errors {
          error!("Task {} failed but errors ignored. Error: {}", self.name, e);

          Ok("".to_owned())
        } else {
          return Err(ExecutorError::TaskFailed(e.to_string()));
        }
      },
    };

    debug!("Completed task {}", self.name);

    result
  }

  fn log_info(&self, message: String) {
    if self.cmd_type != CmdType::Internal {
      info!("{}", message);
    }
  }

  async fn check_sources(&self, fingerprint: Arc<Db>) -> ExecutorResult<bool> {
    let source_strategy: Box<dyn SourceStrategy + Send> = match self.source_strategy {
      SourceMethod::Hash => Box::new(HashSource::new(Arc::clone(&fingerprint))),
      SourceMethod::Timestamp => Box::new(TimestampSource::new(Arc::clone(&fingerprint))),
    };

    if let Some(sources) = &self.sources {
      source_strategy.is_changed(sources.clone()).await
    } else {
      Ok(false)
    }
  }
}

#[async_trait]
pub trait SourceStrategy: Send {
  async fn is_changed(&self, sources: Vec<String>) -> ExecutorResult<bool>;
}

impl Identifiable for TaskNode {
  fn id(&self) -> String {
    self.name.clone()
  }
}

impl TaskItem for TaskNode {
  fn run_mode(&self) -> RunMode {
    self.run_mode.clone()
  }
}

#[async_trait]
impl Executable<TaskNode> for TaskNode {
  /// Stores the result of a dependent task
  async fn set_result(&self, task_name: String, res: String) {
    let mut deps_res = self.deps_res.lock().await;

    deps_res.insert(task_name, res);
  }

  /// Executes the task and returns the result
  async fn execute(
    &self,
    cache: Arc<Mutex<IndexMap<String, CacheItem>>>,
    fingerprint: Arc<Db>,
  ) -> ExecutorResult<String> {
    if !self.check_sources(fingerprint.clone()).await? {
      self.log_info(format!("All files are up to date"));

      return Ok("".to_string());
    };

    self.log_info(format!("Starting task {}", self.name));

    // Debug information about dependency results
    if enabled!(Level::DEBUG) {
      let deps = self.deps_res.lock().await;

      for (name, res) in &*deps {
        debug!("Dependency {} results: {}", name, res);
      }
    }

    if let Some(dag) = self.dag.clone() {
      let executor = Executor::new(dag, ExecutorConfig::default(), Some(cache.clone()), fingerprint.clone())?;
      let results = executor.execute(self.cancel_token.clone()).await?;

      return Ok(results.join("\n"));
    }

    let result = match (&self.cmd, &self.tpl) {
      // This variant should validate on load octafile stage
      (Some(_), Some(_)) => unreachable!(),
      (Some(cmd), None) => self.execute_cmd(cmd.clone(), cache).await,
      (None, Some(tpl)) => {
        let rendered_cmd = self.render_template(tpl, Some(cache)).await?;

        Ok(rendered_cmd)
      },
      // This variant just if we want only run deps
      (None, None) => Ok("".to_string()),
    }?;

    Ok(result)
  }
}
