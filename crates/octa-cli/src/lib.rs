use std::{path::PathBuf, sync::Arc};

use clap::{CommandFactory, Parser};
use logger::{ChronoLocal, OctaFormatter};
use tokio::signal;
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

const OCTA_DATA_DIR: &str = ".octa";

#[derive(Parser)]
#[clap(author, version, about, bin_name("octa"), name("octa"), propagate_version(true))]
pub(crate) struct Cli {
  pub commands: Option<Vec<String>>,

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

  #[arg(last = true)]
  task_args: Vec<String>,
}

struct ExecuteItem {
  executor: Executor<TaskNode>,
  command: String,
}

pub async fn run() -> OctaResult<()> {
  // Parse command line arguments
  let args = Cli::parse();

  // Load environments
  let _ = dotenvy::dotenv();

  // Configure the subscriber with a custom format layer
  let filter_layer = EnvFilter::try_from_default_env()
    .or_else(|_| {
      if args.verbose {
        EnvFilter::try_new("debug")
      } else {
        EnvFilter::try_new("info")
      }
    })
    .unwrap();

  // Create formatting layer
  let fmt_layer = fmt::layer()
    .compact()
    .with_timer(ChronoLocal)
    .with_file(false)
    .with_line_number(false)
    .with_span_events(FmtSpan::CLOSE)
    .event_format(OctaFormatter);

  // Combine layers and set as global default
  tracing_subscriber::registry().with(filter_layer).with(fmt_layer).init();

  // Load octafile
  let octafile = Octafile::load(args.config, args.global)?;

  if args.dry {
    warn!("Octa run in dry mode");
  }

  let cancel_token = CancellationToken::new();
  // Start task for catching interrupt
  tokio::spawn({
    let cancel_token = cancel_token.clone();
    async move {
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
    }
  });

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

  let fingerprint = Arc::new(sled::open(format!("{}/fingerprint", OCTA_DATA_DIR))?);

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
      .build(
        Arc::clone(&octafile),
        command,
        cancel_token.clone(),
        args.task_args.clone(),
      )
      .await?;

    let executor = Executor::new(
      dag,
      ExecutorConfig { silent: false },
      None,
      Arc::clone(&fingerprint),
      args.dry,
      Some(summary.clone()),
    )?;
    tasks.push(ExecuteItem {
      executor,
      command: command.to_string(),
    });
  }

  if args.parallel {
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

  summary.print().await;

  Ok(())
}
