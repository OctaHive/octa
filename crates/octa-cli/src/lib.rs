use std::{
  collections::HashMap,
  env,
  fs::File,
  io::{self, IsTerminal, Read},
  num::NonZeroUsize,
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
};

use clap::{CommandFactory, Parser};
use clap_complete::aot::{generate, Generator, Shell};
use dialoguer::{Input, Password, Select};
use lazy_static::lazy_static;
use logger::{ChronoLocal, OctaFormatter};
use octa_plugin::{protocol::Schema, SHELL_CAPABILITY};
use octa_plugin_manager::plugin_manager::PluginManager;
use serde::Deserialize;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout, Duration};
use tokio::{signal, sync::Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{
  fmt::{self, format::FmtSpan},
  prelude::*,
  EnvFilter,
};

use error::{OctaError, OctaResult};
use octa_executor::{
  executor::ExecutorConfig,
  summary::Summary,
  vars::{VariablePrompt, VariableResolver},
  watcher::{SourceWatcher, WatchTarget},
  Executor, TaskGraphBuilder, TaskNode,
};
use octa_finder::OctaFinder;
use octa_octafile::{Octafile, SyntheticInclude, WatchInterval};

mod error;
mod logger;

const SHELL_PLUGIN_NAME: &str = "shell";
const TEMPLATE_PLUGIN_NAME: &str = "tpl";
const BUILTIN_PLUGIN_NAMES: [&str; 2] = [SHELL_PLUGIN_NAME, TEMPLATE_PLUGIN_NAME];
const DEFAULT_TASK: &str = "default";
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_millis(100);

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

lazy_static! {
  static ref OCTA_DATA_DIR: String = env::var("OCTA_CACHE_DIR").unwrap_or_else(|_| ".octa".to_string());
}

#[derive(Parser)]
#[clap(author, version, about, bin_name("octa"), name("octa"), propagate_version(true))]
pub(crate) struct Cli {
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

  #[arg(short = 'e', long = "env-file", value_name = "PATH")]
  pub env_files: Vec<PathBuf>,

  /// Override an Octafile variable with a string value
  #[arg(long = "var", value_name = "NAME=VALUE", value_parser = parse_cli_var)]
  pub vars: Vec<(String, String)>,

  #[arg(short, long, default_value_t = false)]
  pub parallel: bool,

  /// Maximum number of tasks that may run at the same time
  #[arg(long, value_name = "N")]
  pub concurrency: Option<NonZeroUsize>,

  #[arg(short, long, default_value_t = false)]
  pub verbose: bool,

  #[arg(short, long, default_value_t = false)]
  pub list_tasks: bool,

  /// Search available tasks by qualified name or description
  #[arg(long, value_name = "QUERY", conflicts_with = "commands")]
  pub search: Option<String>,

  #[arg(short, long, default_value_t = false)]
  pub dry: bool,

  #[arg(short, long, default_value_t = false)]
  pub global: bool,

  #[arg(long, default_value_t = false)]
  pub clean_cache: bool,

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

fn generate_completions<G: Generator>(gen: G, cmd: &mut clap::Command) {
  let bin_name = cmd.get_name().to_string();
  generate(gen, cmd, bin_name, &mut io::stdout());
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

struct ExecuteItem {
  executor: Executor<TaskNode>,
  command: String,
}

struct ExecutionOptions {
  parallel: bool,
  dry: bool,
  force: bool,
  failfast: bool,
  vars: Vec<(String, String)>,
  task_args: Vec<String>,
}

#[derive(Clone)]
struct ExecutionContext {
  plugin_manager: Arc<PluginManager>,
  octafile: Arc<Octafile>,
  fingerprint: Arc<sled::Db>,
  summary: Arc<Summary>,
  concurrency: Option<Arc<Semaphore>>,
  variable_resolver: Option<Arc<dyn VariableResolver>>,
}

fn concurrency_limiter(cli: Option<NonZeroUsize>, configured: Option<NonZeroUsize>) -> Option<Arc<Semaphore>> {
  cli.or(configured).map(|limit| Arc::new(Semaphore::new(limit.get())))
}

struct TerminalVariableResolver {
  // Equivalent requirements reuse answers across dependencies, commands and watch rebuilds.
  values: Mutex<HashMap<VariablePrompt, String>>,
}

impl TerminalVariableResolver {
  fn new() -> Self {
    Self {
      values: Mutex::new(HashMap::new()),
    }
  }

  fn read(&self, prompt: &VariablePrompt) -> Result<String, String> {
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
}

impl VariableResolver for TerminalVariableResolver {
  fn resolve(&self, prompt: &VariablePrompt) -> Result<String, String> {
    let mut values = self.values.lock().map_err(|error| error.to_string())?;
    if let Some(value) = values.get(prompt) {
      return Ok(value.clone());
    }

    let value = self.read(prompt)?;
    values.insert(prompt.clone(), value.clone());
    Ok(value)
  }
}

/// Sets up signal handling for graceful shutdown
async fn setup_signal_handling(cancel_token: CancellationToken) {
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
            info!("Received Ctrl-C, shutting down...");
            cancel_token.cancel()
        },
        _ = terminate => {
            info!("Received terminate, shutting down...");
            cancel_token.cancel()
        },
    }
  });
}

/// Sets up logging based on verbosity and test environment
fn setup_logging(verbose: bool) -> OctaResult<()> {
  let filter_layer = EnvFilter::try_from_default_env()
    .or_else(|_| {
      if verbose {
        EnvFilter::try_new("debug")
      } else {
        EnvFilter::try_new("info")
      }
    })
    .unwrap();

  let pretty_print = env::var("OCTA_TESTS").is_err();
  if pretty_print {
    let fmt_layer = fmt::layer()
      .compact()
      .with_timer(ChronoLocal)
      .with_file(false)
      .with_line_number(false)
      .with_span_events(FmtSpan::CLOSE)
      .event_format(OctaFormatter);

    tracing_subscriber::registry().with(filter_layer).with(fmt_layer).init();
  } else {
    let fmt_layer = fmt::layer()
      .compact()
      .with_file(false)
      .with_level(false)
      .without_time()
      .with_target(false)
      .with_line_number(false)
      .with_span_events(FmtSpan::CLOSE);

    tracing_subscriber::registry().with(filter_layer).with(fmt_layer).init();
  }
  Ok(())
}

/// Initializes plugin manager and loads plugins
async fn initialize_plugins(
  plugin_manager: Arc<PluginManager>,
  config_plugins: Vec<String>,
) -> OctaResult<(Arc<PluginManager>, HashMap<String, Schema>)> {
  let mut plugin_futures = Vec::new();
  let plugins = [
    config_plugins,
    BUILTIN_PLUGIN_NAMES.iter().map(|name| name.to_string()).collect(),
  ]
  .concat();

  // Start all plugins in parallel
  for plugin in plugins {
    #[cfg(not(windows))]
    let plugin_name = format!("octa_plugin_{}", plugin);
    #[cfg(windows)]
    let plugin_name = format!("octa_plugin_{}.exe", plugin);

    let plugin_manager = plugin_manager.clone();
    let plugin_key = plugin.clone();

    let future = tokio::spawn(async move {
      match timeout(PLUGIN_TIMEOUT, plugin_manager.start_plugin(&plugin_name)).await {
        Ok(Ok(schema)) => Ok((plugin_key, schema)),
        Ok(Err(e)) => Err(OctaError::PluginStartError(format!("Plugin error: {}", e))),
        Err(_) => Err(OctaError::PluginStartError(format!("Plugin timeout: {}", plugin_name))),
      }
    });

    plugin_futures.push(future);
  }

  // Collect results
  let mut plugin_keys = HashMap::new();
  for future in plugin_futures {
    match future.await {
      Ok(Ok((plugin, schema))) => {
        plugin_keys.insert(plugin, schema);
      },
      Ok(Err(e)) => return Err(e),
      Err(e) => return Err(OctaError::Runtime(e.to_string())),
    }
  }

  Ok((plugin_manager, plugin_keys))
}

/// Resolves the task-type key used for short commands from configuration or the shell capability.
fn resolve_default_plugin(configured: Option<String>, schemas: &HashMap<String, Schema>) -> OctaResult<String> {
  if let Some(key) = configured {
    if schemas.values().any(|schema| schema.key == key) {
      return Ok(key);
    }

    return Err(OctaError::ConfigLoadError(format!(
      "unknown default plugin task type '{key}'"
    )));
  }

  schemas
    .values()
    .find(|schema| {
      schema
        .capabilities
        .iter()
        .any(|capability| capability == SHELL_CAPABILITY)
    })
    // Plugins built against the previous protocol did not advertise capabilities.
    .or_else(|| schemas.get(SHELL_PLUGIN_NAME))
    .map(|schema| schema.key.clone())
    .ok_or_else(|| OctaError::PluginStartError("no plugin provides the shell capability".to_string()))
}

/// Executes tasks either in parallel or sequentially
async fn execute_tasks(
  tasks: Vec<ExecuteItem>,
  parallel: bool,
  failfast: bool,
  cancel_token: CancellationToken,
) -> OctaResult<()> {
  // A batch token lets CLI fail-fast cancel sibling commands without cancelling a watch loop
  // or another caller that owns the outer token.
  let batch_token = cancel_token.child_token();

  if parallel {
    let mut handles = JoinSet::new();
    let mut first_error = None;

    for task in tasks {
      let task_token = batch_token.clone();
      handles.spawn(async move { task.executor.execute(task_token, &task.command).await });
    }

    // Join in completion order so the first failure can interrupt siblings immediately.
    while let Some(result) = handles.join_next().await {
      let error = match result {
        Ok(Ok(_)) => continue,
        Ok(Err(error)) => OctaError::ExecutionError(error),
        Err(error) => OctaError::Runtime(error.to_string()),
      };

      if first_error.is_none() {
        if failfast {
          batch_token.cancel();
        }
        first_error = Some(error);
      }
    }

    if let Some(error) = first_error {
      return Err(error);
    }
  } else {
    for task in tasks {
      task.executor.execute(batch_token.clone(), &task.command).await?;
    }
  }
  Ok(())
}

async fn build_execute_items(
  context: &ExecutionContext,
  commands: &[String],
  options: &ExecutionOptions,
) -> OctaResult<(Vec<ExecuteItem>, Vec<WatchTarget>)> {
  let mut tasks = Vec::with_capacity(commands.len());
  let mut watch_targets = Vec::new();

  for command in commands {
    let mut builder =
      TaskGraphBuilder::new(context.plugin_manager.clone())?.with_variable_overrides(options.vars.clone());
    if let Some(resolver) = &context.variable_resolver {
      builder = builder.with_variable_resolver(resolver.clone());
    }
    let dag = builder
      .build(
        Arc::clone(&context.octafile),
        command,
        options.parallel,
        options.task_args.clone(),
      )
      .await?;

    watch_targets.extend(dag.nodes().iter().filter_map(|node| node.watch_target()));

    let executor = Executor::new(
      context.plugin_manager.clone(),
      dag,
      ExecutorConfig {
        silent: false,
        failfast: options.failfast,
        concurrency: context.concurrency.clone(),
      },
      None,
      Arc::clone(&context.fingerprint),
      options.dry,
      options.force,
      Some(context.summary.clone()),
    )?;
    tasks.push(ExecuteItem {
      executor,
      command: command.clone(),
    });
  }

  Ok((tasks, watch_targets))
}

fn tasks_request_watch(octafile: &Arc<Octafile>, commands: &[String]) -> bool {
  let finder = OctaFinder::new();
  commands.iter().any(|command| {
    finder
      .find_by_path(Arc::clone(octafile), command)
      .iter()
      .any(|result| result.task.watch.unwrap_or(false))
  })
}

fn qualify_monorepo_commands(commands: Vec<String>, namespace: Option<&[String]>) -> Vec<String> {
  let Some(namespace) = namespace.filter(|namespace| !namespace.is_empty()) else {
    return commands;
  };
  let prefix = namespace.join(":");

  commands
    .into_iter()
    .map(|command| {
      if command.contains(':') {
        command
      } else {
        format!("{prefix}:{command}")
      }
    })
    .collect()
}

async fn execute_watch(
  context: ExecutionContext,
  commands: &[String],
  options: &ExecutionOptions,
  interval: Duration,
  cancel_token: CancellationToken,
) -> OctaResult<()> {
  let (tasks, targets) = build_execute_items(&context, commands, options).await?;

  if targets.is_empty() {
    return Err(OctaError::WatchSourcesMissing);
  }

  let mut watcher = tokio::select! {
    biased;
    _ = cancel_token.cancelled() => return Ok(()),
    watcher = SourceWatcher::new(targets, cancel_token.clone()) => watcher?,
  };
  if let Err(error) = execute_tasks(tasks, options.parallel, options.failfast, cancel_token.clone()).await {
    warn!("Task execution failed; waiting for source changes: {}", error);
  }

  info!("Watching sources for changes");
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
      info!("Sources changed; restarting tasks");
      let (tasks, _) = build_execute_items(&context, commands, options).await?;

      if let Err(error) = execute_tasks(tasks, options.parallel, options.failfast, cancel_token.clone()).await {
        warn!("Task execution failed; waiting for source changes: {}", error);
      }
    }
  }

  Ok(())
}

pub async fn run() -> OctaResult<()> {
  // Parse command line arguments
  let mut args = Cli::parse();

  extract_inline_vars(&mut args)?;

  if let Some(shell) = args.completions {
    let mut cmd = Cli::command();
    generate_completions(shell, &mut cmd);
    return Ok(());
  }

  load_env_files(&args.env_files)?;
  setup_logging(args.verbose)?;

  let plugins_dir = std::env::var("OCTA_PLUGINS_DIR").unwrap_or_else(|_| "plugins".to_string());
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));

  let config = match args.config {
    Some(config) => load_config(config)?,
    None => PluginConfig::default(),
  };

  let (plugin_manager, plugin_schemas) = initialize_plugins(plugin_manager.clone(), config.plugins).await?;
  let default_plugin = resolve_default_plugin(config.default_plugin, &plugin_schemas)?;

  let mut validation_schemas = HashMap::new();
  for schema in plugin_schemas.values() {
    if validation_schemas
      .insert(schema.key.clone(), schema.validation_schema.clone())
      .is_some()
    {
      return Err(OctaError::PluginStartError(format!(
        "more than one plugin provides the '{}' task type",
        schema.key
      )));
    }
  }

  let fingerprint = Arc::new(sled::open(format!("{}/fingerprint", *OCTA_DATA_DIR))?);
  if args.clean_cache {
    fingerprint.clear()?;
    octa_monorepo::clear_cache(&fingerprint)?;
    plugin_manager.shutdown_all().await;
    return Ok(());
  }

  let entry_path = Octafile::resolve_path(args.octafile.clone(), args.global, args.dir.clone())?;
  let working_dir = match &args.dir {
    Some(path) if path.is_absolute() => path.clone(),
    Some(path) => env::current_dir()?.join(path),
    None => env::current_dir()?,
  };
  let monorepo = octa_monorepo::resolve(
    &entry_path,
    &working_dir,
    args.octafile.is_some() || args.global,
    &fingerprint,
  )?;
  let synthetic_includes = monorepo
    .projects
    .iter()
    .map(|project| SyntheticInclude {
      namespace: project.namespace.clone(),
      path: project.octafile.clone(),
    })
    .collect::<Vec<_>>();
  if !synthetic_includes.is_empty() {
    info!(
      "Loaded {} monorepo projects{}",
      synthetic_includes.len(),
      if monorepo.cache_hit { " from cache" } else { "" }
    );
  }

  let octafile = Octafile::load_with_schemas_vars_and_includes_from(
    Some(monorepo.root_octafile),
    false,
    None,
    validation_schemas,
    default_plugin,
    &args.vars,
    &synthetic_includes,
  )?;

  if args.dry {
    warn!("Octa run in dry mode");
  }

  let cancel_token = CancellationToken::new();
  setup_signal_handling(cancel_token.clone()).await;

  if args.list_tasks || args.search.is_some() {
    let finder = OctaFinder::new();
    let commands = match args.search.as_deref() {
      Some(query) => finder.search(Arc::clone(&octafile), query),
      None => finder.find_by_path(Arc::clone(&octafile), "**"),
    };
    let filtered = commands.into_iter().filter(|cmd| !cmd.task.internal.unwrap_or(false));
    let found_commands: Vec<(String, Option<String>)> = filtered.map(|c| (c.name.clone(), c.task.desc)).collect();

    for (name, description) in found_commands.into_iter().rev() {
      if let Some(description) = description {
        println!("{}: {}", name, description);
      } else {
        println!("{}", name);
      }
    }

    return Ok(());
  }

  let use_default_task = args.commands.is_none();
  let commands = qualify_monorepo_commands(
    args.commands.unwrap_or_else(|| vec![DEFAULT_TASK.to_string()]),
    monorepo.current_namespace.as_deref(),
  );
  if use_default_task
    && OctaFinder::new()
      .find_by_path(Arc::clone(&octafile), &commands[0])
      .is_empty()
  {
    Cli::command().print_help().unwrap();
    println!();

    return Ok(());
  }

  let options = ExecutionOptions {
    parallel: args.parallel,
    dry: args.dry,
    force: args.force,
    failfast: args.failfast,
    vars: args.vars,
    task_args: args.task_args,
  };
  let summary = Arc::new(Summary::new());
  let variable_resolver: Option<Arc<dyn VariableResolver>> =
    (!args.non_interactive && io::stdin().is_terminal() && io::stderr().is_terminal())
      .then(|| Arc::new(TerminalVariableResolver::new()) as Arc<dyn VariableResolver>);
  let execution_context = ExecutionContext {
    plugin_manager: plugin_manager.clone(),
    octafile: Arc::clone(&octafile),
    fingerprint: Arc::clone(&fingerprint),
    summary: summary.clone(),
    concurrency: concurrency_limiter(args.concurrency, octafile.concurrency),
    variable_resolver,
  };
  let watch = args.watch || tasks_request_watch(&octafile, &commands);

  if watch {
    let interval = if let Some(interval) = args.interval {
      interval
    } else if let Some(interval) = octafile.interval {
      interval.duration()
    } else {
      DEFAULT_WATCH_INTERVAL
    };

    execute_watch(
      execution_context.clone(),
      &commands,
      &options,
      interval,
      cancel_token.clone(),
    )
    .await?;
  } else {
    let (tasks, _) = build_execute_items(&execution_context, &commands, &options).await?;
    execute_tasks(tasks, options.parallel, options.failfast, cancel_token).await?;
  }

  if args.summary {
    summary.print().await;
  }

  plugin_manager.shutdown_all().await;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs::{self, File};
  use std::io::Write;
  use std::path::PathBuf;
  use tempfile::TempDir;

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

  fn glob_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
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

  #[test]
  fn cli_concurrency_overrides_octafile_default() {
    let configured = NonZeroUsize::new(2);
    let cli = NonZeroUsize::new(4);

    assert_eq!(
      concurrency_limiter(None, None).map(|limit| limit.available_permits()),
      None
    );
    assert_eq!(
      concurrency_limiter(None, configured).map(|limit| limit.available_permits()),
      Some(2)
    );
    assert_eq!(
      concurrency_limiter(cli, configured).map(|limit| limit.available_permits()),
      Some(4)
    );
  }

  #[test]
  fn qualifies_only_bare_commands_inside_a_monorepo_project() {
    let namespace = ["packages".to_owned(), "api".to_owned()];
    let commands = vec![
      "build".to_owned(),
      "*".to_owned(),
      "packages:web:build".to_owned(),
      "::root".to_owned(),
    ];

    assert_eq!(
      qualify_monorepo_commands(commands, Some(&namespace)),
      ["packages:api:build", "packages:api:*", "packages:web:build", "::root"]
    );
    assert_eq!(qualify_monorepo_commands(vec!["build".to_owned()], None), ["build"]);
  }

  #[test]
  fn terminal_variable_resolver_reuses_values() {
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
      .unwrap()
      .insert(prompt.clone(), "production".to_owned());
    assert_eq!(resolver.resolve(&prompt), Ok("production".to_owned()));

    let constrained = VariablePrompt {
      enum_values: Some(vec!["development".to_owned(), "production".to_owned()]),
      ..prompt
    };
    assert!(!resolver.values.lock().unwrap().contains_key(&constrained));
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

  #[test]
  fn task_watch_only_applies_to_direct_cli_selection() {
    let temp_dir = TempDir::new().unwrap();
    fs::write(
      temp_dir.path().join("Octafile.yml"),
      r#"
version: 1

tasks:
  watched:
    watch: true
    sources:
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

    let octafile = Octafile::load(
      Some(temp_dir.path().join("Octafile.yml")),
      false,
      vec!["shell".to_string()],
      "shell",
    )
    .unwrap();

    assert!(tasks_request_watch(&octafile, &["watched".to_string()]));
    assert!(!tasks_request_watch(&octafile, &["dependency".to_string()]));
    assert!(!tasks_request_watch(&octafile, &["command".to_string()]));
  }

  #[tokio::test]
  async fn test_watch_reruns_task_after_source_change() {
    let temp_dir = TempDir::new().unwrap();
    let source = temp_dir.path().join("source.txt");
    let output = temp_dir.path().join("runs.txt");
    fs::write(&source, "initial").unwrap();
    fs::write(
      temp_dir.path().join("Octafile.yml"),
      format!(
        r#"
version: 1
interval: 25ms

tasks:
  build:
    watch: true
    sources:
      - "{}"
    shell: echo run >> runs.txt
"#,
        glob_path(&source),
      ),
    )
    .unwrap();

    let plugin_manager = Arc::new(PluginManager::new(test_plugins_dir()));
    let (plugin_manager, schemas) = initialize_plugins(plugin_manager, Vec::new()).await.unwrap();
    let octafile = Octafile::load(
      Some(temp_dir.path().join("Octafile.yml")),
      false,
      schemas.values().map(|schema| schema.key.clone()).collect(),
      "shell",
    )
    .unwrap();
    let commands = vec!["build".to_string()];
    assert!(tasks_request_watch(&octafile, &commands));

    let cancel_token = CancellationToken::new();
    let watch_cancel_token = cancel_token.clone();
    let watch_plugin_manager = plugin_manager.clone();
    let fingerprint = Arc::new(sled::Config::new().temporary(true).open().unwrap());
    let handle = tokio::spawn(async move {
      let options = ExecutionOptions {
        parallel: false,
        dry: false,
        force: false,
        failfast: false,
        vars: Vec::new(),
        task_args: Vec::new(),
      };
      execute_watch(
        ExecutionContext {
          plugin_manager: watch_plugin_manager,
          octafile,
          fingerprint,
          summary: Arc::new(Summary::new()),
          concurrency: None,
          variable_resolver: None,
        },
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
    plugin_manager.shutdown_all().await;
  }

  #[tokio::test]
  async fn test_watch_requires_sources() {
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

    let octafile = Octafile::load(
      Some(temp_dir.path().join("Octafile.yml")),
      false,
      vec!["shell".to_string()],
      "shell",
    )
    .unwrap();
    let options = ExecutionOptions {
      parallel: false,
      dry: false,
      force: false,
      failfast: false,
      vars: Vec::new(),
      task_args: Vec::new(),
    };
    let result = execute_watch(
      ExecutionContext {
        plugin_manager: Arc::new(PluginManager::new(temp_dir.path())),
        octafile,
        fingerprint: Arc::new(sled::Config::new().temporary(true).open().unwrap()),
        summary: Arc::new(Summary::new()),
        concurrency: None,
        variable_resolver: None,
      },
      &["build".to_string()],
      &options,
      Duration::from_millis(25),
      CancellationToken::new(),
    )
    .await;

    assert!(matches!(result, Err(OctaError::WatchSourcesMissing)));
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
  fn test_resolve_default_plugin_uses_configured_task_type() {
    let schemas = HashMap::from([
      (
        "custom-shell-executable".to_string(),
        Schema {
          key: "shell-command".to_string(),
          capabilities: vec![SHELL_CAPABILITY.to_owned()],
          validation_schema: None,
        },
      ),
      (
        "custom".to_string(),
        Schema {
          key: "docker".to_string(),
          capabilities: Vec::new(),
          validation_schema: None,
        },
      ),
    ]);

    assert_eq!(resolve_default_plugin(None, &schemas).unwrap(), "shell-command");
    assert_eq!(
      resolve_default_plugin(Some("docker".to_string()), &schemas).unwrap(),
      "docker"
    );
    assert!(resolve_default_plugin(Some("missing".to_string()), &schemas).is_err());

    let legacy = HashMap::from([(
      SHELL_PLUGIN_NAME.to_owned(),
      Schema {
        key: "legacy-shell".to_owned(),
        capabilities: Vec::new(),
        validation_schema: None,
      },
    )]);
    assert_eq!(resolve_default_plugin(None, &legacy).unwrap(), "legacy-shell");
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
