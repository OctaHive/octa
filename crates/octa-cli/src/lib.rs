use std::path::PathBuf;

use clap::Parser;
use error::OctaResult;
use octa_executor::{Executor, TaskGraphBuilder};
use octa_octafile::Octafile;

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

  // Create DAG
  let builder = TaskGraphBuilder::new();
  let dag = builder.build(octafile, &args.command)?;

  // Print graph
  if args.print_graph {
    dag.print_graph();
  }

  let executor = Executor::new(dag);
  executor.execute().await?;

  Ok(())
}
