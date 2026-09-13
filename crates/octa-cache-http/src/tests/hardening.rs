//! Retry classification and cancellation tests for active transfers.

use std::{
  pin::Pin,
  task::{Context, Poll},
};

use super::*;
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::oneshot;

/// Reader that proves upload staging polled its body before remaining pending.
struct SignallingPendingReader(Option<oneshot::Sender<()>>);

impl AsyncRead for SignallingPendingReader {
  fn poll_read(
    mut self: Pin<&mut Self>,
    _context: &mut Context<'_>,
    _buffer: &mut ReadBuf<'_>,
  ) -> Poll<std::io::Result<()>> {
    if let Some(started) = self.0.take() {
      let _ = started.send(());
    }
    Poll::Pending
  }
}

#[tokio::test]
async fn http_statuses_have_explicit_retry_and_cache_semantics() {
  let action = Digest::blake3(b"status-action");

  let (missing, missing_requests) = TestServer::counting_status(StatusCode::NOT_FOUND).await;
  assert!(
    HttpCacheStore::new(HttpCacheConfig::loopback(&missing.endpoint()), CancellationToken::new())
      .unwrap()
      .get_action("project", &action)
      .await
      .unwrap()
      .is_none()
  );
  assert_eq!(missing_requests.load(Ordering::Relaxed), 1);

  for status in [
    StatusCode::UNAUTHORIZED,
    StatusCode::FORBIDDEN,
    StatusCode::PRECONDITION_FAILED,
  ] {
    let (server, requests) = TestServer::counting_status(status).await;
    let mut config = HttpCacheConfig::loopback(&server.endpoint());
    config.policy.max_retries = 2;
    let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
    assert!(client.get_action("project", &action).await.is_err());
    assert_eq!(requests.load(Ordering::Relaxed), 1, "HTTP {status} was retried");
  }

  for status in [
    StatusCode::TOO_MANY_REQUESTS,
    StatusCode::INTERNAL_SERVER_ERROR,
    StatusCode::BAD_GATEWAY,
    StatusCode::SERVICE_UNAVAILABLE,
    StatusCode::GATEWAY_TIMEOUT,
  ] {
    let (server, requests) = TestServer::counting_status(status).await;
    let mut config = HttpCacheConfig::loopback(&server.endpoint());
    config.policy.max_retries = 1;
    config.policy.retry_base_delay = Duration::ZERO;
    config.policy.retry_max_delay = Duration::from_millis(1);
    let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
    assert!(client.get_action("project", &action).await.is_err());
    assert_eq!(
      requests.load(Ordering::Relaxed),
      2,
      "HTTP {status} was not retried once"
    );
  }
}

#[tokio::test]
async fn cancellation_interrupts_active_download_and_upload_staging() {
  let (download_server, requests) = TestServer::hanging_blob().await;
  let cancellation = CancellationToken::new();
  let client = HttpCacheStore::new(
    HttpCacheConfig::loopback(&download_server.endpoint()),
    cancellation.clone(),
  )
  .unwrap();
  let blob = fixture().0;
  let download = tokio::spawn(async move { client.read_blob(&blob).await });
  tokio::time::timeout(Duration::from_secs(2), async {
    while requests.load(Ordering::Relaxed) == 0 {
      tokio::task::yield_now().await;
    }
  })
  .await
  .expect("download request did not reach the fixture server");
  cancellation.cancel();
  assert!(matches!(
    download.await.unwrap(),
    Err(octa_cache::CacheError::Cancelled)
  ));

  let upload_server = ReferenceServer::start().await;
  let cancellation = CancellationToken::new();
  let client = HttpCacheStore::new(
    HttpCacheConfig::loopback(&upload_server.endpoint()),
    cancellation.clone(),
  )
  .unwrap();
  let (started_tx, started_rx) = oneshot::channel();
  let blob = fixture().0;
  let upload = tokio::spawn(async move {
    client
      .write_blob_if_absent(&blob, Box::pin(SignallingPendingReader(Some(started_tx))))
      .await
  });
  tokio::time::timeout(Duration::from_secs(2), started_rx)
    .await
    .expect("upload staging did not poll its source")
    .expect("upload staging dropped its source before polling");
  cancellation.cancel();
  assert!(matches!(upload.await.unwrap(), Err(octa_cache::CacheError::Cancelled)));
  assert_eq!(
    upload_server.request_count(),
    0,
    "a partial staged body reached the server"
  );
}
