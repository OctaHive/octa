#[tokio::main]
async fn main() {
  if let Err(e) = octa_cli::run().await {
    println!("{:?}", e);
  }
}
