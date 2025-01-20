use std::{
  collections::HashMap,
  env,
  fs::File,
  io::{self, Read},
  path::{Path, PathBuf},
  sync::Arc,
};

use clap::{CommandFactory, Parser};
use clap_complete::aot::{generate, Generator, Shell};
use lazy_static::lazy_static;
use logger::{ChronoLocal, OctaFormatter};
use octa_plugin::protocol::Schema;
use octa_plugin_manager::plugin_manager::PluginManager;
use serde::Deserialize;
use tokio::signal;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use tracing_subscriber::{
  fmt::{self, format::FmtSpan},
  prelude::*,
  EnvFilter,
};

use error::{OctaError, OctaResult};
use octa_executor::{executor::ExecutorConfig, summary::Summary, Executor, TaskGraphBuilder, TaskNode};
use octa_finder::OctaFinder;
use octa_octafile::Octafile;

mod error;
mod logger;

const DEFAULT_PLUGINS: [&str; 2] = ["shell", "tpl"];
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Deserialize)]
struct PluginConfig {
  plugins: Vec<String>,
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
  pub commands: Option<Vec<String>>,

  #[arg(short, long)]
  pub octafile: Option<PathBuf>,

  #[arg(short, long)]
  pub config: Option<PathBuf>,

  #[arg(short, long, default_value_t = false)]
  pub parallel: bool,

  #[arg(short, long, default_value_t = false)]
  pub verbose: bool,

  #[arg(short, long, default_value_t = false)]
  pub list_tasks: bool,

  #[arg(short, long, default_value_t = false)]
  pub dry: bool,

  #[arg(short, long, default_value_t = false)]
  pub global: bool,

  #[arg(long, default_value_t = false)]
  pub clean_cache: bool,

  #[arg(long, default_value_t = false)]
  pub summary: bool,

  #[arg(short, long, default_value_t = false)]
  pub force: bool,

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

struct ExecuteItem {
  executor: Executor<TaskNode>,
  command: String,
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
  let plugins = [config_plugins, DEFAULT_PLUGINS.iter().map(|s| s.to_string()).collect()].concat();

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

/// Executes tasks either in parallel or sequentially
async fn execute_tasks(tasks: Vec<ExecuteItem>, parallel: bool, cancel_token: CancellationToken) -> OctaResult<()> {
  if parallel {
    let mut handles = Vec::with_capacity(tasks.len());
    let mut results = Vec::with_capacity(tasks.len());

    // Spawn all tasks
    for task in tasks {
      let cancel_token = cancel_token.clone();
      let handle = tokio::spawn(async move { task.executor.execute(cancel_token, &task.command).await });
      handles.push(handle);
    }

    // Wait for all tasks to complete
    for handle in handles {
      match handle.await {
        Ok(result) => results.push(result),
        Err(e) => return Err(error::OctaError::Runtime(e.to_string())),
      }
    }

    // Check if any task failed
    for result in results {
      if let Err(e) = result {
        return Err(OctaError::ExecutionError(e));
      }
    }
  } else {
    for task in tasks {
      task.executor.execute(cancel_token.clone(), &task.command).await?;
    }
  }
  Ok(())
}

pub async fn run() -> OctaResult<()> {
  // Parse command line arguments
  let args = Cli::parse();

  if let Some(shell) = args.completions {
    let mut cmd = Cli::command();
    generate_completions(shell, &mut cmd);
    return Ok(());
  }

  // Load environments
  let _ = dotenvy::dotenv();
  setup_logging(args.verbose)?;

  let plugins_dir = std::env::var("OCTA_PLUGINS_DIR").unwrap_or_else(|_| "plugins".to_string());
  let plugin_manager = Arc::new(PluginManager::new(plugins_dir));

  let config_plugins = match args.config {
    Some(config) => load_config(config)?.plugins,
    None => vec![],
  };

  let (plugin_manager, plugin_keys) = initialize_plugins(plugin_manager.clone(), config_plugins).await?;

  // Load octafile
  let octafile = Octafile::load(args.octafile, args.global, plugin_keys.keys().cloned().collect())?;

  if args.dry {
    warn!("Octa run in dry mode");
  }

  let cancel_token = CancellationToken::new();
  setup_signal_handling(cancel_token.clone()).await;

  if args.list_tasks {
    let finder = OctaFinder::new();
    let commands = finder.find_by_path(Arc::clone(&octafile), "**");
    let filtered = commands.into_iter().filter(|cmd| !cmd.task.internal.unwrap_or(false));
    let found_commands: Vec<(String, Option<String>)> = filtered.map(|c| (c.name.clone(), c.task.desc)).collect();

    for cmd in found_commands.into_iter().rev() {
      if cmd.1.is_none() {
        println!("{}", cmd.0);
      } else {
        println!("{}: {}", cmd.0, cmd.1.unwrap());
      }
    }

    return Ok(());
  }

  let fingerprint = Arc::new(sled::open(format!("{}/fingerprint", *OCTA_DATA_DIR))?);

  if args.clean_cache {
    fingerprint.clear()?;

    return Ok(());
  }

  if args.commands.is_none() {
    Cli::command().print_help().unwrap();
    println!();

    return Ok(());
  }

  let summary = Arc::new(Summary::new());
  let mut tasks = vec![];
  for command in args.commands.as_ref().unwrap() {
    // Create DAG
    let builder = TaskGraphBuilder::new()?;
    let dag = builder
      .build(Arc::clone(&octafile), command, args.parallel, args.task_args.clone())
      .await?;

    let executor = Executor::new(
      plugin_manager.clone(),
      dag,
      ExecutorConfig { silent: false },
      None,
      Arc::clone(&fingerprint),
      args.dry,
      args.force,
      Some(summary.clone()),
    )?;
    tasks.push(ExecuteItem {
      executor,
      command: command.to_string(),
    });
  }

  execute_tasks(tasks, args.parallel, cancel_token).await?;

  if args.summary {
    summary.print().await;
  }

  plugin_manager.shutdown_all().await;

  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::fs::File;
  use std::io::Write;
  use std::path::PathBuf;
  use tempfile::TempDir;

  fn create_test_config(dir: &TempDir, content: &str) -> PathBuf {
    let config_path = dir.path().join("config.yml");
    let mut file = File::create(&config_path).unwrap();
    write!(file, "{}", content).unwrap();
    config_path
  }

  #[test]
  fn test_cli_parse() {
    let cli = Cli::parse_from(&["octa", "--parallel", "build"]);
    assert!(cli.parallel);
    assert_eq!(cli.commands, Some(vec!["build".to_string()]));
  }

  #[test]
  fn test_load_config() {
    let temp_dir = TempDir::new().unwrap();
    let config_content = r#"
      plugins:
        - "plugin1"
        - "plugin2"
    "#;
    let config_path = create_test_config(&temp_dir, config_content);

    let config = load_config(config_path).unwrap();
    assert_eq!(config.plugins, vec!["plugin1", "plugin2"]);
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
    let cli = Cli::parse_from(&["octa", "build", "--", "--release"]);
    assert_eq!(cli.task_args, vec!["--release"]);
  }

  #[test]
  fn test_cli_multiple_commands() {
    let cli = Cli::parse_from(&["octa", "test", "build"]);
    assert_eq!(cli.commands, Some(vec!["test".to_string(), "build".to_string()]));
  }

  #[test]
  fn test_cli_dry_run() {
    let cli = Cli::parse_from(&["octa", "--dry", "build"]);
    assert!(cli.dry);
  }

  #[test]
  fn test_cli_verbose() {
    let cli = Cli::parse_from(&["octa", "--verbose", "build"]);
    assert!(cli.verbose);
  }

  #[test]
  fn test_cli_completions() {
    let cli = Cli::parse_from(&["octa", "--completions", "bash"]);
    assert_eq!(cli.completions, Some(Shell::Bash));
  }

  #[test]
  fn test_cli_global() {
    let cli = Cli::parse_from(&["octa", "--global", "build"]);
    assert!(cli.global);
  }
}
