use std::{path::PathBuf, sync::Arc};

use octa_finder::OctaFinder;
use tokio::signal;
use tokio_util::sync::CancellationToken;

use clap::Parser;
use error::OctaResult;
use octa_executor::{executor::ExecutorConfig, Executor, TaskGraphBuilder};
use octa_octafile::Octafile;
use tracing::info;

mod error;

#[derive(Parser)]
#[clap(author, version, about, bin_name("octo"), propagate_version(true))]
pub(crate) struct Cli {
  pub command: String,

  #[arg(short, long)]
  pub config: Option<PathBuf>,

  #[arg(short, long, default_value_t = false)]
  pub print_graph: bool,

  #[arg(short, long, default_value_t = false)]
  pub verbose: bool,

  #[arg(short, long, default_value_t = false)]
  pub list_tasks: bool,

  #[arg(short, long, default_value_t = false)]
  pub global: bool,
}

pub async fn run() -> OctaResult<()> {
  // Parse command line arguments
  let args = Cli::parse();

  // Load environments
  dotenvy::dotenv()?;

  // Initialize logging
  let mut level = tracing::Level::INFO;
  if args.verbose {
    level = tracing::Level::DEBUG;
  }
  tracing_subscriber::fmt().with_max_level(level).init();

  // Load octafile
  let octafile = Octafile::load(args.config, args.global)?;

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
    let filtered = commands
      .into_iter()
      .filter(|cmd| cmd.task.internal.unwrap_or(false) != true);
    let found_commands: Vec<String> = filtered.map(|c| c.name.clone()).collect();

    for cmd in found_commands.into_iter().rev() {
      println!("{}", cmd);
    }

    return Ok(());
  }

  // Create DAG
  let builder = TaskGraphBuilder::new()?;
  let dag = builder.build(octafile, &args.command, cancel_token.clone())?;

  // Print graph
  if args.print_graph {
    dag.print_graph();
  }

  let fingerprint = Arc::new(sled::open(".octa/fingerprint")?);
  let executor = Executor::new(
    dag,
    ExecutorConfig {
      show_summary: true,
      silent: false,
    },
    None,
    fingerprint,
  )?;
  executor.execute(cancel_token.clone()).await?;

  Ok(())
}
