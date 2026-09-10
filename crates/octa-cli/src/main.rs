#[tokio::main]
async fn main() {
  // The Windows main thread has a smaller stack than Tokio workers. Poll the
  // application future on a worker so debug builds do not overflow that stack.
  if !tokio::spawn(octa_cli::run_and_report()).await.unwrap_or(false) {
    std::process::exit(1);
  }
}
