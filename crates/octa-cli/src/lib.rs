use std::path::PathBuf;

use tokio::signal;
use tokio_util::sync::CancellationToken;

use clap::Parser;
use error::OctaResult;
use octa_executor::{Executor, TaskGraphBuilder};
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
  let octafile = Octafile::load(args.config)?;

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

  // Create DAG
  let builder = TaskGraphBuilder::new();
  let dag = builder.build(octafile, &args.command, cancel_token.clone())?;

  // Print graph
  if args.print_graph {
    dag.print_graph();
  }

  let executor = Executor::new(dag);
  executor.execute(cancel_token.clone()).await?;

  Ok(())
}
