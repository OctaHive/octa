//! Interactive command-line frontend for loading and executing Octa tasks.
//!
//! Argument parsing and terminal presentation live here; reusable workspace
//! composition and execution remain in `octa-runtime`.

use std::{
  collections::HashMap,
  env,
  fs::File,
  io::{self, IsTerminal, Read},
  num::NonZeroUsize,
  path::{Path, PathBuf},
  sync::Arc,
};

use async_trait::async_trait;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::aot::{generate, Generator, Shell};
use dialoguer::{Input, Password, Select};
pub use logger::ConsoleLayer;
use serde::Deserialize;
#[cfg(test)]
use tokio::time::timeout;
use tokio::time::{sleep, Duration};
use tokio::{signal, sync::Mutex};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{prelude::*, EnvFilter};

use error::{OctaError, OctaResult};
use octa_executor::{SourceWatcher, VariablePrompt, VariableResolver};
use octa_finder::OctaFinder;
use octa_octafile::{Octafile, OctafileError, OutputConfig, OutputMode, PresentationConfig, Silence, WatchInterval};
use octa_output::{CliDocument, Console, ConsoleLevel, SummaryItem, TaskListItem};
use octa_runtime::{RunOptions, Runtime, RuntimeCacheConfig, RuntimeConfig};
use presentation::{terminal_console, CiMode};

mod cache_commands;
mod error;
mod logger;
mod presentation;
mod raw_terminal;

const DEFAULT_TASK: &str = "default";
const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_millis(100);

enum DiagnosticsSetup {
  Install,
  Inherit,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginConfig {
  #[serde(default)]
  plugins: Vec<String>,
  default_plugin: Option<String>,
}

fn load_config<P: AsRef<Path>>(config_path: P) -> OctaResult<PluginConfig> {
  let mut file = File::open(config_path).map_err(|e| OctaError::ConfigLoadError(e.to_string()))?;
  let mut contents = String::new();
  file
    .read_to_string(&mut contents)
    .map_err(|e| OctaError::ConfigLoadError(e.to_string()))?;

  let config: PluginConfig = serde_yml::from_str(&contents).map_err(|e| OctaError::ConfigLoadError(e.to_string()))?;
  Ok(config)
}

#[derive(Parser)]
#[clap(
  author,
  version,
  about,
  bin_name("octa"),
  name("octa"),
  propagate_version(true),
  subcommand_precedence_over_arg = true
)]
pub(crate) struct Cli {
  #[command(subcommand)]
  management: Option<ManagementCommand>,

  /// Tasks to run and optional variable overrides
  #[arg(value_name = "TASK|NAME=VALUE")]
  pub commands: Option<Vec<String>>,

  #[arg(short, long)]
  pub octafile: Option<PathBuf>,

  /// Start Octafile discovery from this directory
  #[arg(long, value_name = "PATH", conflicts_with = "global")]
  pub dir: Option<PathBuf>,

  #[arg(short, long)]
  pub config: Option<PathBuf>,

  /// Verify and load plugins from this lock file.
  #[arg(long, value_name = "PATH")]
  pub plugin_lock: Option<PathBuf>,

  /// Resolve logical secret references using this provider profile.
  #[arg(long, value_name = "PATH", env = "OCTA_SECRETS_PROFILE")]
  pub secrets_profile: Option<PathBuf>,

  /// Load machine-specific task-result cache settings from this TOML profile.
  #[arg(long, value_name = "PATH", env = "OCTA_CACHE_PROFILE")]
  pub cache_profile: Option<PathBuf>,

  #[arg(short = 'e', long = "env-file", value_name = "PATH")]
  pub env_files: Vec<PathBuf>,

  /// Override an Octafile variable with a string value
  #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_cli_var)]
  pub vars: Vec<(String, String)>,

  #[arg(short, long, default_value_t = false)]
  pub parallel: bool,

  /// Control how concurrent task output is presented
  #[arg(long, value_name = "MODE", env = "TASK_OUTPUT")]
  output: Option<OutputMode>,

  /// Template printed before a grouped task (supports task variables).
  #[arg(long, value_name = "TEMPLATE", env = "TASK_OUTPUT_GROUP_BEGIN")]
  output_group_begin: Option<String>,

  /// Template printed after a grouped task (supports task variables).
  #[arg(long, value_name = "TEMPLATE", env = "TASK_OUTPUT_GROUP_END")]
  output_group_end: Option<String>,

  /// Print grouped output only for failed or cancelled tasks.
  #[arg(long, value_name = "BOOL", env = "TASK_OUTPUT_GROUP_ERROR_ONLY")]
  output_group_error_only: Option<bool>,

  /// Emit annotations understood by the selected CI provider
  #[arg(long, value_enum, default_value_t)]
  ci: CiMode,

  /// Maximum number of tasks that may run at the same time
  #[arg(long, value_name = "N")]
  pub concurrency: Option<NonZeroUsize>,

  #[arg(short, long, default_value_t = false)]
  pub verbose: bool,

  /// Suppress Octa's own non-error task messages.
  #[arg(short = 'q', long, env = "TASK_QUIET", default_value_t = false)]
  pub quiet: bool,

  /// Suppress both task streams, or only stdout/stderr when specified.
  #[arg(
    long,
    value_name = "STREAM",
    num_args = 0..=1,
    default_missing_value = "true",
    require_equals = true,
    env = "TASK_SILENT"
  )]
  pub silent: Option<Silence>,

  /// Connect task stdin/stdout/stderr through an exclusive PTY session.
  #[arg(short = 'r', long, env = "TASK_RAW", default_value_t = false)]
  pub raw: bool,

  #[arg(short, long, default_value_t = false)]
  pub list_tasks: bool,

  /// Search available tasks by qualified name or description
  #[arg(long, value_name = "QUERY", conflicts_with = "commands")]
  pub search: Option<String>,

  #[arg(short, long, default_value_t = false)]
  pub dry: bool,

  #[arg(short, long, default_value_t = false)]
  pub global: bool,

  /// Clear Octa's workspace discovery state without touching the result cache.
  #[arg(long, default_value_t = false)]
  pub clean_state: bool,

  #[arg(long, default_value_t = false)]
  pub summary: bool,

  /// Never request missing variables interactively
  #[arg(long, default_value_t = false)]
  pub non_interactive: bool,

  #[arg(short, long, default_value_t = false)]
  pub force: bool,

  /// Cancel already running parallel tasks after the first failure
  #[arg(short = 'F', long, default_value_t = false)]
  pub failfast: bool,

  /// Watch source files and rerun selected tasks when they change
  #[arg(short = 'w', long, default_value_t = false)]
  pub watch: bool,

  /// Set the watch polling interval (for example: 250ms, 2s, or 1m)
  #[arg(long, value_name = "DURATION", value_parser = parse_watch_interval)]
  pub interval: Option<Duration>,

  /// Generate shell completions
  #[arg(long)]
  completions: Option<Shell>,

  #[arg(last = true)]
  task_args: Vec<String>,
}

#[derive(Clone, Debug, Subcommand)]
enum ManagementCommand {
  /// Manage reproducible plugin metadata.
  Plugin {
    #[command(subcommand)]
    command: PluginCommand,
  },
  /// Inspect and maintain the local task-result cache.
  Cache {
    #[command(subcommand)]
    command: CacheCommand,
  },
}

#[derive(Clone, Debug, Subcommand)]
enum CacheCommand {
  /// Show the active local cache path and capacity.
  Status,
  /// Remove least-recently-used actions and unreachable blobs.
  Prune,
  /// Resolve exact task context and inspect the cache without running task bodies.
  ///
  /// Variable, secret, and plugin-backed context providers are evaluated because
  /// their resolved values participate in the real action identity.
  Explain {
    /// Task whose cacheable dependency graph should be inspected.
    task: String,
  },
}

#[derive(Clone, Debug, Subcommand)]
enum PluginCommand {
  /// Build Octa.lock from '*.plugin.yml' manifests in the plugin directory.
  Lock {
    #[arg(long, value_name = "PATH", default_value = "Octa.lock")]
    output: PathBuf,
  },
  /// Verify every locked plugin digest, platform, and protocol.
  Verify {
    #[arg(long, value_name = "PATH", default_value = "Octa.lock")]
    lock: PathBuf,
  },
}

fn generate_completions<G: Generator>(gen: G, cmd: &mut clap::Command) -> String {
  let bin_name = cmd.get_name().to_string();
  let mut output = Vec::new();
  generate(gen, cmd, bin_name, &mut output);
  String::from_utf8(output).expect("clap generated non-UTF-8 completions")
}

fn parse_watch_interval(value: &str) -> Result<Duration, String> {
  value.parse::<WatchInterval>().map(WatchInterval::duration)
}

fn parse_cli_var(value: &str) -> Result<(String, String), String> {
  let (name, value) = value
    .split_once('=')
    .ok_or_else(|| "variables must use NAME=VALUE format".to_owned())?;
  if name.is_empty() || name.trim() != name {
    return Err("variable name must not be empty or surrounded by whitespace".to_owned());
  }

  Ok((name.to_owned(), value.to_owned()))
}

fn extract_inline_vars(args: &mut Cli) -> OctaResult<()> {
  let Some(items) = args.commands.take() else {
    return Ok(());
  };

  let mut commands = Vec::with_capacity(items.len());
  for item in items {
    if item.contains('=') {
      args
        .vars
        .push(parse_cli_var(&item).map_err(OctaError::InvalidVariable)?);
    } else {
      commands.push(item);
    }
  }

  args.commands = (!commands.is_empty()).then_some(commands);
  Ok(())
}

fn load_env_files(paths: &[PathBuf]) -> OctaResult<()> {
  if paths.is_empty() {
    let _ = dotenvy::dotenv();
    return Ok(());
  }

  for path in paths.iter().rev() {
    dotenvy::from_path(path).map_err(|source| OctaError::Dotenv {
      path: path.display().to_string(),
      source,
    })?;
  }

  Ok(())
}

struct TerminalVariableResolver {
  // Equivalent requirements reuse answers across dependencies, commands and watch rebuilds.
  values: Mutex<HashMap<VariablePrompt, String>>,
  // Terminal dialogs are blocking and must never overlap.
  prompt: Mutex<()>,
}

impl TerminalVariableResolver {
  fn new() -> Self {
    Self {
      values: Mutex::new(HashMap::new()),
      prompt: Mutex::new(()),
    }
  }

  fn read(prompt: &VariablePrompt) -> Result<String, String> {
    // Enum options are already part of the Octafile, so selecting one does not expose a runtime secret.
    if let Some(enum_values) = &prompt.enum_values {
      let selected = Select::new()
        .with_prompt(&prompt.question)
        .items(enum_values)
        .interact()
        .map_err(|error| error.to_string())?;
      return Ok(enum_values[selected].clone());
    }

    if prompt.secret {
      return Password::new()
        .with_prompt(&prompt.question)
        .interact()
        .map_err(|error| error.to_string());
    }

    Input::<String>::new()
      .with_prompt(&prompt.question)
      .interact_text()
      .map_err(|error| error.to_string())
  }

  async fn resolve_with<R>(&self, prompt: &VariablePrompt, read: R) -> Result<String, String>
  where
    R: FnOnce(VariablePrompt) -> Result<String, String> + Send + 'static,
  {
    if let Some(value) = self.values.lock().await.get(prompt).cloned() {
      return Ok(value);
    }

    let _prompt_guard = self.prompt.lock().await;
    if let Some(value) = self.values.lock().await.get(prompt).cloned() {
      return Ok(value);
    }

    let prompt_key = prompt.clone();
    let dialog_prompt = prompt.clone();
    let value = tokio::task::spawn_blocking(move || read(dialog_prompt))
      .await
      .map_err(|error| error.to_string())??;
    self.values.lock().await.insert(prompt_key, value.clone());
    Ok(value)
  }
}

#[async_trait]
impl VariableResolver for TerminalVariableResolver {
  async fn resolve(&self, prompt: &VariablePrompt) -> Result<String, String> {
    self.resolve_with(prompt, |prompt| Self::read(&prompt)).await
  }
}

/// Sets up signal handling for graceful shutdown
async fn setup_signal_handling(cancel_token: CancellationToken, console: Arc<Console>) {
  tokio::spawn(async move {
    let ctrl_c = async {
      signal::ctrl_c().await.expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
      signal::unix::signal(signal::unix::SignalKind::terminate())
        .expect("failed to install signal handler")
        .recv()
        .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            let _ = console.message(ConsoleLevel::Info, "Received Ctrl-C, shutting down...").await;
            cancel_token.cancel()
        },
        _ = terminate => {
            let _ = console.message(ConsoleLevel::Info, "Received terminate, shutting down...").await;
            cancel_token.cancel()
        },
    }
  });
}

/// Routes internal diagnostics through the selected output renderer.
fn setup_logging(console: &Arc<Console>, verbose: bool) -> OctaResult<()> {
  let filter_layer = EnvFilter::try_from_default_env()
    .or_else(|_| {
      if verbose {
        EnvFilter::try_new("debug")
      } else {
        EnvFilter::try_new("info")
      }
    })
    .unwrap();

  tracing_subscriber::registry()
    .with(filter_layer)
    .with(ConsoleLayer::new(console))
    .try_init()
    .map_err(|error| OctaError::Runtime(format!("failed to initialize diagnostics: {error}")))
}

async fn execute_watch(
  runtime: Arc<Runtime>,
  commands: &[String],
  options: &RunOptions,
  interval: Duration,
  cancel_token: CancellationToken,
) -> OctaResult<()> {
  let (tasks, targets) = runtime.prepare(commands, options).await?;

  if targets.is_empty() {
    return Err(OctaError::WatchSourcesMissing);
  }

  let mut watcher = tokio::select! {
    biased;
    _ = cancel_token.cancelled() => return Ok(()),
    watcher = SourceWatcher::with_snapshotter(targets, runtime.input_snapshotter(), cancel_token.clone()) => watcher?,
  };
  if let Err(error) = run_prepared(&runtime, tasks, options).await {
    runtime
      .console()
      .message(
        ConsoleLevel::Warn,
        format!("Task execution failed; waiting for source changes: {error}"),
      )
      .await?;
  }

  runtime
    .console()
    .message(ConsoleLevel::Info, "Watching task inputs for changes")
    .await?;
  loop {
    tokio::select! {
      _ = cancel_token.cancelled() => break,
      _ = sleep(interval) => {},
    }

    let changed = tokio::select! {
      biased;
      _ = cancel_token.cancelled() => break,
      changed = watcher.poll() => changed?,
    };
    if changed {
      runtime
        .console()
        .message(ConsoleLevel::Info, "Sources changed; restarting tasks")
        .await?;
      let (tasks, _) = runtime.prepare(commands, options).await?;

      if let Err(error) = run_prepared(&runtime, tasks, options).await {
        runtime
          .console()
          .message(
            ConsoleLevel::Warn,
            format!("Task execution failed; waiting for source changes: {error}"),
          )
          .await?;
      }
    }
  }

  Ok(())
}

async fn run_prepared(
  runtime: &Runtime,
  tasks: Vec<octa_executor::PreparedExecution>,
  options: &RunOptions,
) -> OctaResult<()> {
  let results = runtime.execute(tasks, options.parallel, options.failfast).await?;
  if let Some(failure) = results.into_iter().find_map(|result| result.into_failure()) {
    return Err(Box::new(failure).into());
  }
  Ok(())
}

/// Parses process arguments and runs one CLI invocation to completion.
pub async fn run() -> OctaResult<()> {
  let args = Cli::parse();
  let presentation = configured_presentation(&args)?;
  let console = terminal_console(
    presentation.output,
    args.ci,
    presentation.quiet,
    presentation.force_output_mode,
    presentation.adaptive_output,
  );
  run_with_console_and_diagnostics(console, args).await
}

/// Runs the CLI and reports its terminal error through the same output pipeline.
pub async fn run_and_report() -> bool {
  let args = Cli::parse();
  let presentation = match configured_presentation(&args) {
    Ok(config) => config,
    Err(error) => {
      let console = terminal_console(OutputConfig::default(), args.ci, args.quiet, false, true);
      let _ = console
        .document(CliDocument::Failure {
          message: error.to_string(),
        })
        .await;
      return false;
    },
  };
  let console = terminal_console(
    presentation.output,
    args.ci,
    presentation.quiet,
    presentation.force_output_mode,
    presentation.adaptive_output,
  );
  match run_with_console_and_diagnostics(console.clone(), args).await {
    Ok(()) => true,
    Err(error) => {
      let _ = console
        .document(CliDocument::Failure {
          message: error.to_string(),
        })
        .await;
      false
    },
  }
}

struct ConfiguredPresentation {
  output: OutputConfig,
  quiet: bool,
  force_output_mode: bool,
  adaptive_output: bool,
}

fn configured_presentation(args: &Cli) -> OctaResult<ConfiguredPresentation> {
  let configured = match Octafile::resolve_path(args.octafile.clone(), args.global, args.dir.clone()) {
    Ok(path) => Octafile::read_presentation_config(path)?,
    Err(OctafileError::NotSearchedError | OctafileError::NotFoundError(_)) => PresentationConfig::default(),
    Err(error) => return Err(error.into()),
  };
  let adaptive_output = configured.output.is_none() && args.output.is_none();
  let mut output = configured.output.unwrap_or_else(|| OutputConfig {
    mode: if args.parallel && !args.raw {
      OutputMode::Prefixed
    } else {
      OutputMode::Interleaved
    },
    ..OutputConfig::default()
  });
  if let Some(mode) = args.output {
    output.mode = mode;
  }
  let has_group_override =
    args.output_group_begin.is_some() || args.output_group_end.is_some() || args.output_group_error_only.is_some();
  if has_group_override && output.mode != OutputMode::Group {
    return Err(OctaError::InvalidOutputConfig(
      "group begin/end/error-only options require '--output group'".to_owned(),
    ));
  }
  if let Some(begin) = &args.output_group_begin {
    output.group.begin = Some(begin.clone());
  }
  if let Some(end) = &args.output_group_end {
    output.group.end = Some(end.clone());
  }
  if let Some(error_only) = args.output_group_error_only {
    output.group.error_only = error_only;
  }
  if output.mode == OutputMode::Group {
    for template in [output.group.begin.as_deref(), output.group.end.as_deref()]
      .into_iter()
      .flatten()
    {
      octa_output::validate_output_template(template)
        .map_err(|error| OctaError::InvalidOutputConfig(error.to_string()))?;
    }
  }
  if args.raw && output.mode == OutputMode::Json {
    return Err(OctaError::InvalidOutputConfig(
      "raw/PTY mode cannot be combined with JSON output".to_owned(),
    ));
  }
  Ok(ConfiguredPresentation {
    output,
    quiet: args.quiet || configured.quiet.unwrap_or(false),
    force_output_mode: args.output.is_some(),
    adaptive_output,
  })
}

/// Runs the CLI with an injected renderer and leaves the process-global tracing subscriber untouched.
///
/// Embedders that want tracing diagnostics in this console can install [`ConsoleLayer`]
/// in their existing subscriber. The injected renderer controls presentation, including grouping.
/// The standalone binary installs its tracing layer and selects a renderer from the CLI options.
pub async fn run_with_console(console: Arc<Console>) -> OctaResult<()> {
  run_with_console_and_mode(console, DiagnosticsSetup::Inherit, Cli::parse()).await
}

async fn run_with_console_and_diagnostics(console: Arc<Console>, args: Cli) -> OctaResult<()> {
  run_with_console_and_mode(console, DiagnosticsSetup::Install, args).await
}

async fn run_with_console_and_mode(console: Arc<Console>, diagnostics: DiagnosticsSetup, args: Cli) -> OctaResult<()> {
  let result = run_with_console_mode(console.clone(), diagnostics, args).await;
  let drain_result = console.drain().await;

  match result {
    Err(error) => Err(error),
    Ok(()) => drain_result.map_err(Into::into),
  }
}

async fn run_with_console_mode(console: Arc<Console>, diagnostics: DiagnosticsSetup, mut args: Cli) -> OctaResult<()> {
  extract_inline_vars(&mut args)?;

  if let Some(shell) = args.completions {
    let mut cmd = Cli::command();
    let text = generate_completions(shell, &mut cmd);
    console.document(CliDocument::Completion { text }).await?;
    return Ok(());
  }

  load_env_files(&args.env_files)?;
  if matches!(diagnostics, DiagnosticsSetup::Install) {
    setup_logging(&console, args.verbose)?;
  }

  let config = match args.config.as_ref() {
    Some(config) => load_config(config)?,
    None => PluginConfig::default(),
  };
  let workspace = match &args.dir {
    Some(path) if path.is_absolute() => path.clone(),
    Some(path) => env::current_dir()?.join(path),
    None => env::current_dir()?,
  };
  let data_dir = PathBuf::from(env::var_os("OCTA_DATA_DIR").unwrap_or_else(|| ".octa".into()));
  if args.clean_state {
    let data_dir = if data_dir.is_absolute() {
      data_dir
    } else {
      workspace.join(data_dir)
    };
    octa_monorepo::clear_cache(&data_dir.join("monorepo"))?;
    return Ok(());
  }

  let variable_resolver: Option<Arc<dyn VariableResolver>> =
    (!args.non_interactive && io::stdin().is_terminal() && io::stderr().is_terminal())
      .then(|| Arc::new(TerminalVariableResolver::new()) as Arc<dyn VariableResolver>);
  let plugins_dir = PathBuf::from(std::env::var_os("OCTA_PLUGINS_DIR").unwrap_or_else(|| "plugins".into()));
  let cache_command = match &args.management {
    Some(ManagementCommand::Cache { command }) => Some(command),
    _ => None,
  };
  let cache_explain = cache_commands::explain_task(cache_command);
  match &args.management {
    Some(ManagementCommand::Plugin { command }) => {
      return run_plugin_management(command, &workspace, &plugins_dir, &console).await;
    },
    Some(ManagementCommand::Cache { command }) if cache_explain.is_none() => {
      return cache_commands::run_management(command, &workspace, args.cache_profile.as_deref(), &console).await;
    },
    _ => {},
  }
  let result_cache = args
    .cache_profile
    .as_deref()
    .map(|path| RuntimeCacheConfig::load_profile(path, &workspace))
    .transpose()?;
  if cache_explain.is_some() && result_cache.is_none() {
    return Err(OctaError::CacheProfileRequired);
  }
  let cancellation = CancellationToken::new();
  setup_signal_handling(cancellation.clone(), console.clone()).await;
  let runtime = Arc::new(
    Runtime::load(RuntimeConfig {
      workspace,
      octafile: args.octafile.clone(),
      global: args.global,
      data_dir,
      plugins_dir,
      plugin_lock: args.plugin_lock.clone(),
      secrets_profile: args.secrets_profile.clone(),
      result_cache,
      plugins: config.plugins,
      default_plugin: config.default_plugin,
      variables: args.vars.clone(),
      concurrency: args.concurrency,
      variable_resolver,
      raw_terminal: Arc::new(raw_terminal::LocalRawTerminal),
      console: console.clone(),
      cancellation,
    })
    .await?,
  );

  if runtime.monorepo_project_count() > 0 {
    console
      .message(
        ConsoleLevel::Info,
        format!(
          "Loaded {} monorepo projects{}",
          runtime.monorepo_project_count(),
          if runtime.monorepo_cache_hit() {
            " from cache"
          } else {
            ""
          }
        ),
      )
      .await?;
  }

  let result = match cache_explain {
    Some(task) => cache_commands::run_explain(runtime.clone(), &console, task, &args).await,
    None => run_loaded_runtime(runtime.clone(), console, args).await,
  };
  runtime.shutdown().await;
  result
}

async fn run_plugin_management(
  command: &PluginCommand,
  workspace: &Path,
  plugins_dir: &Path,
  console: &Console,
) -> OctaResult<()> {
  use octa_plugin_manager::plugin_lock::{
    load_plugin_lock, lock_from_manifest_directory, verify_plugin_lock, write_plugin_lock,
  };

  let plugins_dir = if plugins_dir.is_absolute() {
    plugins_dir.to_path_buf()
  } else {
    workspace.join(plugins_dir)
  };
  match command {
    PluginCommand::Lock { output } => {
      let output = if output.is_absolute() {
        output.clone()
      } else {
        workspace.join(output)
      };
      let lock = lock_from_manifest_directory(&plugins_dir).await?;
      write_plugin_lock(&lock, &output)?;
      console
        .message(
          ConsoleLevel::Info,
          format!("Locked {} plugins in {}", lock.plugins.len(), output.display()),
        )
        .await?;
    },
    PluginCommand::Verify { lock } => {
      let lock_path = if lock.is_absolute() {
        lock.clone()
      } else {
        workspace.join(lock)
      };
      let lock = load_plugin_lock(&lock_path)?;
      verify_plugin_lock(&lock, &plugins_dir).await?;
      console
        .message(
          ConsoleLevel::Info,
          format!("Verified {} locked plugins", lock.plugins.len()),
        )
        .await?;
    },
  }
  Ok(())
}

async fn run_loaded_runtime(runtime: Arc<Runtime>, console: Arc<Console>, args: Cli) -> OctaResult<()> {
  if args.dry {
    console.message(ConsoleLevel::Warn, "Octa run in dry mode").await?;
  }

  let cancel_token = runtime.cancellation();

  if args.list_tasks || args.search.is_some() {
    let finder = OctaFinder::new();
    let commands = match args.search.as_deref() {
      Some(query) => finder.search(runtime.octafile().clone(), query),
      None => finder.find_by_path(runtime.octafile().clone(), "**"),
    };
    let filtered = commands.into_iter().filter(|cmd| !cmd.task.internal.unwrap_or(false));
    let found_commands: Vec<(String, Option<String>)> = filtered.map(|c| (c.name.clone(), c.task.desc)).collect();

    let tasks = found_commands
      .into_iter()
      .rev()
      .map(|(name, description)| TaskListItem { name, description })
      .collect();
    console.document(CliDocument::TaskList { tasks }).await?;

    return Ok(());
  }

  let use_default_task = args.commands.is_none();
  let commands = runtime.qualify_commands(args.commands.unwrap_or_else(|| vec![DEFAULT_TASK.to_string()]));
  if use_default_task
    && OctaFinder::new()
      .find_by_path(runtime.octafile().clone(), &commands[0])
      .is_empty()
  {
    let help = Cli::command().render_help().to_string();
    console
      .document(CliDocument::Help {
        text: format!("{help}\n"),
      })
      .await?;

    return Ok(());
  }

  let options = RunOptions {
    parallel: args.parallel && !args.raw,
    dry: args.dry,
    force: args.force,
    failfast: args.failfast,
    variables: args.vars,
    task_args: args.task_args,
    quiet: args.quiet,
    silence: args.silent,
    raw: args.raw,
    cache_probe: false,
  };
  let watch = args.watch || runtime.commands_request_watch(&commands);

  if watch {
    let interval = if let Some(interval) = args.interval {
      interval
    } else if let Some(interval) = runtime.octafile().interval {
      interval.duration()
    } else {
      DEFAULT_WATCH_INTERVAL
    };

    execute_watch(runtime.clone(), &commands, &options, interval, cancel_token.clone()).await?;
  } else {
    let (tasks, _) = runtime.prepare(&commands, &options).await?;
    run_prepared(&runtime, tasks, &options).await?;
  }

  if args.summary {
    let report = runtime.summary().report().await;
    console
      .document(CliDocument::Summary {
        tasks: report
          .tasks
          .into_iter()
          .map(|item| SummaryItem {
            name: item.name,
            duration: item.duration,
          })
          .collect(),
        total: report.total,
      })
      .await?;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use octa_output::{ConsoleDiagnostic, ConsoleEntry, ConsoleRecord};
  use std::fs::{self, File};
  use std::io::Write;
  use std::path::PathBuf;
  use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex as StdMutex,
  };
  use tempfile::TempDir;

  #[derive(Clone)]
  struct RecordingRenderer(Arc<StdMutex<Vec<ConsoleRecord>>>);

  impl octa_output::ConsoleRenderer for RecordingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.lock().unwrap().push(entry.record().clone());
      Ok(())
    }
  }

  fn create_test_config(dir: &TempDir, content: &str) -> PathBuf {
    let config_path = dir.path().join("config.yml");
    let mut file = File::create(&config_path).unwrap();
    write!(file, "{}", content).unwrap();
    config_path
  }

  fn test_plugins_dir() -> PathBuf {
    let target_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug");
    #[cfg(windows)]
    let plugin_names = ["octa_plugin_shell.exe", "octa_plugin_tpl.exe"];
    #[cfg(not(windows))]
    let plugin_names = ["octa_plugin_shell", "octa_plugin_tpl"];

    if plugin_names.iter().all(|name| target_dir.join(name).is_file()) {
      target_dir
    } else {
      PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins")
    }
  }

  async fn test_runtime(temp_dir: &TempDir, console: Arc<Console>) -> Arc<Runtime> {
    Arc::new(
      Runtime::load(RuntimeConfig::headless(
        temp_dir.path().to_path_buf(),
        test_plugins_dir(),
        PathBuf::from(".octa-test"),
        console,
      ))
      .await
      .unwrap(),
    )
  }

  async fn wait_for_lines(path: &Path, expected: usize) {
    timeout(Duration::from_secs(5), async {
      loop {
        let lines = fs::read_to_string(path)
          .map(|content| content.lines().count())
          .unwrap_or_default();
        if lines >= expected {
          break;
        }
        sleep(Duration::from_millis(25)).await;
      }
    })
    .await
    .unwrap();
  }

  async fn wait_for_message(events: &StdMutex<Vec<ConsoleRecord>>, text: &str, expected: usize) {
    timeout(Duration::from_secs(5), async {
      loop {
        let count = events
          .lock()
          .unwrap()
          .iter()
          .filter(|record| {
            matches!(
              record,
              ConsoleRecord::Diagnostic(ConsoleDiagnostic { message, .. }) if message.contains(text)
            )
          })
          .count();
        if count >= expected {
          break;
        }
        sleep(Duration::from_millis(25)).await;
      }
    })
    .await
    .unwrap();
  }

  #[test]
  fn test_cli_parse() {
    let cli = Cli::parse_from([
      "octa",
      "--parallel",
      "--failfast",
      "--non-interactive",
      "--concurrency",
      "4",
      "build",
    ]);
    assert!(cli.parallel);
    assert!(cli.failfast);
    assert!(cli.non_interactive);
    assert_eq!(cli.concurrency.map(NonZeroUsize::get), Some(4));
    assert_eq!(cli.commands, Some(vec!["build".to_string()]));

    assert!(Cli::try_parse_from(["octa", "--concurrency", "0", "build"]).is_err());
  }

  #[tokio::test]
  async fn terminal_variable_resolver_reuses_values() {
    let prompt = VariablePrompt {
      name: "ENVIRONMENT".to_owned(),
      question: "Select environment".to_owned(),
      enum_values: None,
      secret: false,
    };
    let resolver = TerminalVariableResolver::new();
    resolver
      .values
      .lock()
      .await
      .insert(prompt.clone(), "production".to_owned());
    assert_eq!(resolver.resolve(&prompt).await, Ok("production".to_owned()));

    let constrained = VariablePrompt {
      enum_values: Some(vec!["development".to_owned(), "production".to_owned()]),
      ..prompt
    };
    assert!(!resolver.values.lock().await.contains_key(&constrained));
  }

  #[tokio::test]
  async fn terminal_variable_resolver_serializes_and_caches_prompts() {
    fn reader(
      calls: Arc<AtomicUsize>,
      value: &'static str,
    ) -> impl FnOnce(VariablePrompt) -> Result<String, String> + Send + 'static {
      move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(value.to_owned())
      }
    }

    let prompt = VariablePrompt {
      name: "PROFILE".to_owned(),
      question: "Select profile".to_owned(),
      enum_values: None,
      secret: false,
    };
    let resolver = Arc::new(TerminalVariableResolver::new());
    let calls = Arc::new(AtomicUsize::new(0));

    let (first, second) = tokio::join!(
      resolver.resolve_with(&prompt, reader(calls.clone(), "development")),
      resolver.resolve_with(&prompt, reader(calls.clone(), "production")),
    );

    assert_eq!(first.unwrap(), "development");
    assert_eq!(second.unwrap(), "development");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let terminal_prompt = VariablePrompt {
      name: "TERMINAL".to_owned(),
      enum_values: Some(Vec::new()),
      ..prompt.clone()
    };
    assert!(resolver.resolve(&terminal_prompt).await.is_err());

    let failed_prompt = VariablePrompt {
      name: "FAILED".to_owned(),
      ..prompt
    };
    assert_eq!(
      resolver
        .resolve_with(&failed_prompt, |_| Err("input failed".to_owned()))
        .await,
      Err("input failed".to_owned())
    );

    let panicked_prompt = VariablePrompt {
      name: "PANICKED".to_owned(),
      ..failed_prompt
    };
    let error = resolver
      .resolve_with(&panicked_prompt, |_| panic!("prompt reader panicked"))
      .await
      .unwrap_err();
    assert!(error.contains("prompt reader panicked"));
  }

  #[test]
  fn test_cli_watch_options() {
    let cli = Cli::parse_from(["octa", "--watch", "--interval", "250ms", "build"]);

    assert!(cli.watch);
    assert_eq!(cli.interval, Some(Duration::from_millis(250)));
    assert_eq!(parse_watch_interval("2s"), Ok(Duration::from_secs(2)));
    assert_eq!(parse_watch_interval("1m"), Ok(Duration::from_secs(60)));
    assert!(parse_watch_interval("0ms").is_err());
    assert!(parse_watch_interval("100").is_err());
  }

  #[tokio::test]
  async fn task_watch_only_applies_to_direct_cli_selection() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(
      temp_dir.path().join("Octafile.yml"),
      r#"
version: 1

tasks:
  watched:
    watch: true
    files:
      inputs:
        - source.txt
    shell: echo watched

  dependency:
    deps:
      - watched
    shell: echo dependency

  command:
    cmds:
      - task: watched
"#,
    )
    .unwrap();

    let runtime = test_runtime(&temp_dir, Arc::new(Console::default())).await;
    assert!(runtime.commands_request_watch(&["watched".to_string()]));
    assert!(!runtime.commands_request_watch(&["dependency".to_string()]));
    assert!(!runtime.commands_request_watch(&["command".to_string()]));
    runtime.shutdown().await;
  }

  #[tokio::test]
  async fn test_watch_reruns_task_after_source_change() {
    let temp_dir = TempDir::new().unwrap();
    let source = temp_dir.path().join("source.txt");
    let output = temp_dir.path().join("runs.txt");
    fs::write(&source, "initial").unwrap();
    fs::write(
      temp_dir.path().join("Octafile.yml"),
      r#"
version: 1
interval: 25ms

tasks:
  build:
    watch: true
    files:
      inputs:
        - source.txt
    shell: echo run >> runs.txt
"#,
    )
    .unwrap();

    let runtime = test_runtime(&temp_dir, Arc::new(Console::default())).await;
    let commands = vec!["build".to_string()];
    assert!(runtime.commands_request_watch(&commands));

    let cancel_token = CancellationToken::new();
    let watch_cancel_token = cancel_token.clone();
    let watch_runtime = runtime.clone();
    let handle = tokio::spawn(async move {
      let options = RunOptions::default();
      execute_watch(
        watch_runtime,
        &commands,
        &options,
        Duration::from_millis(25),
        watch_cancel_token,
      )
      .await
    });

    wait_for_lines(&output, 1).await;
    fs::write(&source, "changed").unwrap();
    wait_for_lines(&output, 2).await;
    cancel_token.cancel();

    handle.await.unwrap().unwrap();
    runtime.shutdown().await;
  }

  #[tokio::test]
  async fn test_watch_requires_inputs() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(
      temp_dir.path().join("Octafile.yml"),
      r#"
version: 1
tasks:
  build:
    shell: echo build
"#,
    )
    .unwrap();

    let runtime = test_runtime(&temp_dir, Arc::new(Console::default())).await;
    let options = RunOptions::default();
    let result = execute_watch(
      runtime.clone(),
      &["build".to_string()],
      &options,
      Duration::from_millis(25),
      CancellationToken::new(),
    )
    .await;

    assert!(matches!(result, Err(OctaError::WatchSourcesMissing)));
    runtime.shutdown().await;
  }

  #[tokio::test]
  async fn watch_reports_failures_before_and_after_source_changes() {
    let temp_dir = TempDir::new().unwrap();
    let source = temp_dir.path().join("source.txt");
    fs::write(&source, "initial").unwrap();
    fs::write(
      temp_dir.path().join("Octafile.yml"),
      r#"
version: 1
tasks:
  build:
    files:
      inputs:
        - source.txt
    shell: exit 1
"#,
    )
    .unwrap();

    let events = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RecordingRenderer(events.clone())));
    let runtime = test_runtime(&temp_dir, console).await;
    let cancel_token = CancellationToken::new();
    let watch_cancel_token = cancel_token.clone();
    let watch_runtime = runtime.clone();
    let commands = vec!["build".to_owned()];
    let handle = tokio::spawn(async move {
      execute_watch(
        watch_runtime,
        &commands,
        &RunOptions::default(),
        Duration::from_millis(25),
        watch_cancel_token,
      )
      .await
    });

    wait_for_message(&events, "Task execution failed", 1).await;
    let initial_failures = events
      .lock()
      .unwrap()
      .iter()
      .filter(|record| {
        matches!(
          record,
          ConsoleRecord::Diagnostic(ConsoleDiagnostic { message, .. }) if message.contains("Task execution failed")
        )
      })
      .count();
    fs::write(&source, "changed").unwrap();
    wait_for_message(&events, "Sources changed; restarting tasks", 1).await;
    wait_for_message(&events, "Task execution failed", initial_failures + 1).await;
    cancel_token.cancel();

    handle.await.unwrap().unwrap();
    runtime.shutdown().await;
  }

  #[test]
  fn test_cli_env_files() {
    let cli = Cli::parse_from(["octa", "--env-file", ".env.local", "-e", "config/test.env", "build"]);

    assert_eq!(
      cli.env_files,
      vec![PathBuf::from(".env.local"), PathBuf::from("config/test.env")]
    );
  }

  #[test]
  fn test_cli_vars() {
    let cli = Cli::parse_from([
      "octa",
      "--var",
      "PROFILE=development",
      "--var",
      "TOKEN=a=b",
      "--var",
      "EMPTY=",
      "build",
    ]);

    assert_eq!(
      cli.vars,
      vec![
        ("PROFILE".to_owned(), "development".to_owned()),
        ("TOKEN".to_owned(), "a=b".to_owned()),
        ("EMPTY".to_owned(), String::new()),
      ]
    );

    let cli = Cli::parse_from(["octa", "build", "--var", "PROFILE=production"]);
    assert_eq!(cli.vars, vec![("PROFILE".to_owned(), "production".to_owned())]);
  }

  #[test]
  fn test_extract_inline_vars() {
    let mut cli = Cli::parse_from([
      "octa",
      "test",
      "PROFILE=development",
      "build",
      "PROFILE=production",
      "EMPTY=",
      "--",
      "--release",
    ]);
    extract_inline_vars(&mut cli).unwrap();

    assert_eq!(cli.commands, Some(vec!["test".to_owned(), "build".to_owned()]));
    assert_eq!(
      cli.vars,
      vec![
        ("PROFILE".to_owned(), "development".to_owned()),
        ("PROFILE".to_owned(), "production".to_owned()),
        ("EMPTY".to_owned(), String::new()),
      ]
    );
    assert_eq!(cli.task_args, vec!["--release"]);
  }

  #[test]
  fn test_extract_inline_vars_without_commands() {
    let mut cli = Cli::parse_from(["octa", "PROFILE=production"]);
    extract_inline_vars(&mut cli).unwrap();

    assert!(cli.commands.is_none());
    assert_eq!(cli.vars, vec![("PROFILE".to_owned(), "production".to_owned())]);
  }

  #[test]
  fn test_extract_inline_vars_rejects_invalid_assignment() {
    let mut cli = Cli::parse_from(["octa", "=production", "build"]);
    let result = extract_inline_vars(&mut cli);

    assert!(matches!(result, Err(OctaError::InvalidVariable(_))));
  }

  #[test]
  fn test_cli_rejects_invalid_vars() {
    for value in ["PROFILE", "=development", " PROFILE=development"] {
      assert!(Cli::try_parse_from(["octa", "--var", value, "build"]).is_err());
    }
  }

  #[test]
  fn test_load_config() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
      plugins:
        - "plugin1"
        - "plugin2"
      default_plugin: docker
    "#;
    let config_path = create_test_config(&temp_dir, config_content);

    let config = load_config(config_path).unwrap();
    assert_eq!(config.plugins, vec!["plugin1", "plugin2"]);
    assert_eq!(config.default_plugin.as_deref(), Some("docker"));
  }

  #[test]
  fn test_load_config_invalid() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
      invalid_yaml::::
    "#;
    let config_path = create_test_config(&temp_dir, config_content);

    assert!(load_config(config_path).is_err());
  }

  #[test]
  fn test_cli_task_args() {
    let cli = Cli::parse_from(["octa", "build", "--", "--release"]);
    assert_eq!(cli.task_args, vec!["--release"]);
  }

  #[test]
  fn test_cli_multiple_commands() {
    let cli = Cli::parse_from(["octa", "test", "build"]);
    assert_eq!(cli.commands, Some(vec!["test".to_string(), "build".to_string()]));
  }

  #[test]
  fn test_cli_output_mode() {
    let default = Cli::parse_from(["octa", "build"]);
    assert_eq!(default.output, None);

    let grouped = Cli::parse_from(["octa", "--output", "group", "build"]);
    assert_eq!(grouped.output, Some(OutputMode::Group));

    let prefixed = Cli::parse_from(["octa", "--output", "prefixed", "build"]);
    assert_eq!(prefixed.output, Some(OutputMode::Prefixed));

    let on_error = Cli::parse_from(["octa", "--output", "on-error", "build"]);
    assert_eq!(on_error.output, Some(OutputMode::OnError));

    let keep_order = Cli::parse_from(["octa", "--output", "keep-order", "build"]);
    assert_eq!(keep_order.output, Some(OutputMode::KeepOrder));

    let replacing = Cli::parse_from(["octa", "--output", "replacing", "build"]);
    assert_eq!(replacing.output, Some(OutputMode::Replacing));

    let timed = Cli::parse_from(["octa", "--output", "timed", "build"]);
    assert_eq!(timed.output, Some(OutputMode::Timed));

    let json = Cli::parse_from(["octa", "--output", "jsonl", "build"]);
    assert_eq!(json.output, Some(OutputMode::Json));

    assert!(Cli::try_parse_from(["octa", "--output", "unknown", "build"]).is_err());
  }

  #[test]
  fn test_cli_group_output_overrides() {
    let cli = Cli::parse_from([
      "octa",
      "--output",
      "group",
      "--output-group-begin",
      "begin {{.TASK}}",
      "--output-group-end",
      "end",
      "--output-group-error-only",
      "true",
      "build",
    ]);
    assert_eq!(cli.output_group_begin.as_deref(), Some("begin {{.TASK}}"));
    assert_eq!(cli.output_group_end.as_deref(), Some("end"));
    assert_eq!(cli.output_group_error_only, Some(true));
  }

  #[test]
  fn configured_presentation_applies_and_validates_cli_overrides() {
    let group = Cli::parse_from([
      "octa",
      "--output",
      "group",
      "--output-group-begin",
      "begin {{.TASK}}",
      "--output-group-end",
      "end",
      "--output-group-error-only",
      "true",
      "build",
    ]);
    let presentation = configured_presentation(&group).unwrap();
    assert_eq!(presentation.output.group.begin.as_deref(), Some("begin {{.TASK}}"));
    assert_eq!(presentation.output.group.end.as_deref(), Some("end"));
    assert!(presentation.output.group.error_only);

    let wrong_mode = Cli::parse_from(["octa", "--output-group-begin", "begin", "build"]);
    assert!(matches!(
      configured_presentation(&wrong_mode),
      Err(OctaError::InvalidOutputConfig(_))
    ));
    let invalid_template = Cli::parse_from(["octa", "--output", "group", "--output-group-begin", "{{", "build"]);
    assert!(matches!(
      configured_presentation(&invalid_template),
      Err(OctaError::InvalidOutputConfig(_))
    ));
    let raw_json = Cli::parse_from(["octa", "--raw", "--output", "jsonl", "build"]);
    assert!(matches!(
      configured_presentation(&raw_json),
      Err(OctaError::InvalidOutputConfig(_))
    ));
    let parallel = configured_presentation(&Cli::parse_from(["octa", "--parallel", "build"])).unwrap();
    assert_eq!(parallel.output.mode, OutputMode::Prefixed);
  }

  #[test]
  fn test_cli_visibility_and_raw_modes() {
    let all = Cli::parse_from(["octa", "--quiet", "--silent", "--raw", "build"]);
    assert!(all.quiet);
    assert_eq!(all.silent, Some(Silence::All));
    assert!(all.raw);

    let stdout = Cli::parse_from(["octa", "--silent=stdout", "build"]);
    assert_eq!(stdout.silent, Some(Silence::Stdout));

    let stderr = Cli::parse_from(["octa", "--silent=stderr", "build"]);
    assert_eq!(stderr.silent, Some(Silence::Stderr));
  }

  #[test]
  fn test_cli_ci_mode() {
    let default = Cli::parse_from(["octa", "build"]);
    assert_eq!(default.ci, CiMode::Auto);

    let github = Cli::parse_from(["octa", "--ci", "github", "build"]);
    assert_eq!(github.ci, CiMode::Github);
  }

  #[test]
  fn test_cli_dry_run() {
    let cli = Cli::parse_from(["octa", "--dry", "build"]);
    assert!(cli.dry);
  }

  #[test]
  fn test_cli_verbose() {
    let cli = Cli::parse_from(["octa", "--verbose", "build"]);
    assert!(cli.verbose);
  }

  #[test]
  fn test_cli_completions() {
    let cli = Cli::parse_from(["octa", "--completions", "bash"]);
    assert_eq!(cli.completions, Some(Shell::Bash));

    let generated = generate_completions(Shell::Bash, &mut Cli::command());
    assert!(generated.contains("_octa"));
  }

  #[test]
  fn test_cli_global() {
    let cli = Cli::parse_from(["octa", "--global", "build"]);
    assert!(cli.global);
  }

  #[test]
  fn test_cli_dir() {
    let cli = Cli::parse_from(["octa", "--dir", "backend", "build"]);
    assert_eq!(cli.dir, Some(PathBuf::from("backend")));
  }

  #[test]
  fn test_cli_search() {
    let cli = Cli::parse_from(["octa", "--search", "build"]);

    assert_eq!(cli.search.as_deref(), Some("build"));
    assert!(cli.commands.is_none());
  }
}
