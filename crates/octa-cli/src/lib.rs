use std::{path::PathBuf, sync::Arc};

use chrono::Local;
use clap::Parser;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber::{
  fmt::{self, format::FmtSpan, time::FormatTime, FormatFields},
  prelude::*,
  EnvFilter,
};

use error::OctaResult;
use octa_executor::{executor::ExecutorConfig, Executor, TaskGraphBuilder};
use octa_finder::OctaFinder;
use octa_octafile::Octafile;

mod error;

struct ChronoLocal;

impl FormatTime for ChronoLocal {
  fn format_time(&self, w: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
    write!(w, "{}", Local::now().format("%Y-%m-%d %H:%M:%S"))
  }
}

// Custom formatter for adding [octa] prefix
struct OctaFormatter;

impl<'a> FormatFields<'a> for OctaFormatter {
  fn format_fields<R: __tracing_subscriber_field_RecordFields>(
    &self,
    writer: fmt::format::Writer<'a>,
    fields: R,
  ) -> std::fmt::Result {
    let mut writer = writer;
    write!(writer, "[octa] ")?;
    fmt::format::DefaultFields::new().format_fields(writer, fields)
  }
}

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

  #[arg(long, default_value_t = false)]
  pub clean_cache: bool,
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
    .with_level(false)
    .with_target(false)
    .with_timer(ChronoLocal)
    .with_file(false)
    .with_line_number(false)
    .with_span_events(FmtSpan::CLOSE)
    .fmt_fields(OctaFormatter);

  // Combine layers and set as global default
  tracing_subscriber::registry().with(filter_layer).with(fmt_layer).init();

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

    return Ok(());
  }

  let fingerprint = Arc::new(sled::open(".octa/fingerprint")?);

  if args.clean_cache {
    fingerprint.clear()?;

    return Ok(());
  }

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
