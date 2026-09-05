#[tokio::main]
async fn main() {
  if !octa_cli::run_and_report().await {
    std::process::exit(1);
  }
}
