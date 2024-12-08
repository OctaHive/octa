#[tokio::main]
async fn main() {
  if let Err(e) = octa_cli::run().await {
    eprintln!("{:?}", e);

    std::process::exit(1);
  }
}
