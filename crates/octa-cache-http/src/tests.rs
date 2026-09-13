//! In-memory reference service and end-to-end client contract tests.

use std::{
  fs,
  net::SocketAddr,
  sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  },
  time::Duration,
};

use axum::{
  body::Bytes,
  http::{header, HeaderValue, StatusCode},
  response::{IntoResponse, Response},
  Router,
};
use octa_cache::{CacheStore, LayeredCacheStore, LocalCacheConfig, LocalCacheStore, WriteOutcome};
use octa_cache_protocol::{
  ActionResultV1, BlobDescriptor, BlobEncoding, Digest, FindMissingBlobsResponseV1, ACTION_RESULT_VERSION_V1,
  REMOTE_CACHE_BLOB_CONTENT_TYPE, REMOTE_CACHE_JSON_CONTENT_TYPE, REMOTE_CACHE_PROTOCOL_HEADER,
  REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1, REMOTE_CACHE_PROTOCOL_V1,
};
use octa_cache_test_support::ReferenceCache;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_util::sync::CancellationToken;

use crate::{HttpCacheConfig, HttpCacheStore};

struct ReferenceServer {
  address: SocketAddr,
  state: ReferenceCache,
  task: tokio::task::JoinHandle<()>,
}

struct StaticServer {
  address: SocketAddr,
  task: tokio::task::JoinHandle<()>,
}

struct TestServer {
  address: SocketAddr,
  task: tokio::task::JoinHandle<()>,
}

impl TestServer {
  async fn delayed(delay: Duration) -> Self {
    let app = Router::new().fallback(move || async move {
      tokio::time::sleep(delay).await;
      protocol_response(StatusCode::NOT_FOUND, Bytes::new())
    });
    Self::start(app).await
  }

  async fn flaky_body(body: Vec<u8>, content_type: &'static str) -> (Self, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let app = Router::new().fallback(move || {
      let body = body.clone();
      let observed = observed.clone();
      async move {
        let current = observed.fetch_add(1, Ordering::Relaxed);
        if current == 0 {
          let partial = body[..body.len() / 2].to_vec();
          let stream = futures::stream::iter([
            Ok::<_, std::io::Error>(Bytes::from(partial)),
            Err(std::io::Error::other("injected response-body failure")),
          ]);
          let mut response = axum::body::Body::from_stream(stream).into_response();
          response.headers_mut().insert(
            REMOTE_CACHE_PROTOCOL_HEADER,
            HeaderValue::from_static(REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1),
          );
          response.headers_mut().insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&body.len().to_string()).unwrap(),
          );
          response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
          response
        } else {
          typed_response(StatusCode::OK, content_type, body)
        }
      }
    });
    (Self::start(app).await, requests)
  }

  async fn broken_body(body: Vec<u8>, content_type: &'static str) -> Self {
    let app = Router::new().fallback(move || {
      let partial = body[..body.len() / 2].to_vec();
      async move {
        let stream = futures::stream::iter([
          Ok::<_, std::io::Error>(Bytes::from(partial)),
          Err(std::io::Error::other("injected terminal response-body failure")),
        ]);
        let mut response = axum::body::Body::from_stream(stream).into_response();
        response.headers_mut().insert(
          REMOTE_CACHE_PROTOCOL_HEADER,
          HeaderValue::from_static(REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1),
        );
        response
          .headers_mut()
          .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        response
      }
    });
    Self::start(app).await
  }

  async fn retry_after(delay_seconds: u64) -> Self {
    let app = Router::new().fallback(move || async move {
      let mut response = protocol_response(StatusCode::TOO_MANY_REQUESTS, "retry later");
      response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&delay_seconds.to_string()).unwrap(),
      );
      response
    });
    Self::start(app).await
  }

  async fn start(app: Router) -> Self {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Self { address, task }
  }

  fn endpoint(&self) -> String {
    format!("http://{}/", self.address)
  }
}

impl Drop for TestServer {
  fn drop(&mut self) {
    self.task.abort();
  }
}

impl StaticServer {
  async fn start(status: StatusCode, body: Vec<u8>, protocol: bool) -> Self {
    let app = Router::new().fallback(move || {
      let body = body.clone();
      async move {
        let mut response = (status, body).into_response();
        if protocol {
          response.headers_mut().insert(
            REMOTE_CACHE_PROTOCOL_HEADER,
            HeaderValue::from_static(REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1),
          );
          response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(REMOTE_CACHE_JSON_CONTENT_TYPE),
          );
        }
        response
      }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Self { address, task }
  }

  async fn streaming(chunks: Vec<Vec<u8>>, content_type: &'static str) -> Self {
    let app = Router::new().fallback(move || {
      let chunks = chunks.clone();
      async move {
        let stream = futures::stream::iter(chunks.into_iter().map(Ok::<_, std::convert::Infallible>));
        let mut response = axum::body::Body::from_stream(stream).into_response();
        response.headers_mut().insert(
          REMOTE_CACHE_PROTOCOL_HEADER,
          HeaderValue::from_static(REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1),
        );
        response
          .headers_mut()
          .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        response
      }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Self { address, task }
  }

  fn endpoint(&self) -> String {
    format!("http://{}/", self.address)
  }
}

impl Drop for StaticServer {
  fn drop(&mut self) {
    self.task.abort();
  }
}

impl ReferenceServer {
  async fn start() -> Self {
    let state = ReferenceCache::new("fixture-token");
    let app = state.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
      axum::serve(listener, app).await.unwrap();
    });
    Self { address, state, task }
  }

  fn endpoint(&self) -> String {
    format!("http://{}/", self.address)
  }

  fn request_count(&self) -> usize {
    self.state.request_count()
  }

  fn fail_next(&self, count: usize) {
    self.state.fail_next(count);
  }
}

impl Drop for ReferenceServer {
  fn drop(&mut self) {
    self.task.abort();
  }
}

fn protocol_response(status: StatusCode, body: impl Into<axum::body::Body>) -> Response {
  let mut response = (status, body.into()).into_response();
  response.headers_mut().insert(
    REMOTE_CACHE_PROTOCOL_HEADER,
    HeaderValue::from_static(REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1),
  );
  response
}

fn typed_response(status: StatusCode, content_type: &'static str, body: impl Into<axum::body::Body>) -> Response {
  let mut response = protocol_response(status, body);
  response
    .headers_mut()
    .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
  response
}

fn fixture() -> (BlobDescriptor, Vec<u8>, ActionResultV1) {
  let bytes = b"portable-result".to_vec();
  let blob = BlobDescriptor {
    digest: Digest::blake3(&bytes),
    encoding: BlobEncoding::Identity,
    encoded_size_bytes: bytes.len() as u64,
    expanded_size_bytes: bytes.len() as u64,
    entry_count: 1,
  };
  let result = ActionResultV1 {
    result_version: ACTION_RESULT_VERSION_V1,
    action: Digest::blake3(b"action"),
    output_bundle: Some(blob.clone()),
    stdout: Some("cached".to_owned()),
    task_outputs: Default::default(),
    artifacts: Vec::new(),
    reports: Vec::new(),
  };
  (blob, bytes, result)
}

fn client(server: &ReferenceServer) -> HttpCacheStore {
  HttpCacheStore::new(HttpCacheConfig::loopback(&server.endpoint()), CancellationToken::new()).unwrap()
}

fn static_client(server: &StaticServer) -> HttpCacheStore {
  HttpCacheStore::new(HttpCacheConfig::loopback(&server.endpoint()), CancellationToken::new()).unwrap()
}

#[tokio::test]
async fn implements_batch_lookup_streaming_and_create_if_absent() {
  let server = ReferenceServer::start().await;
  let client = client(&server);
  let (blob, bytes, result) = fixture();

  assert_eq!(
    client.find_missing_blobs(std::slice::from_ref(&blob)).await.unwrap(),
    std::slice::from_ref(&blob)
  );
  assert_eq!(
    client
      .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes.clone())))
      .await
      .unwrap(),
    WriteOutcome::Written
  );
  assert!(client
    .find_missing_blobs(std::slice::from_ref(&blob))
    .await
    .unwrap()
    .is_empty());
  assert_eq!(
    client.write_action_if_absent("project", &result).await.unwrap(),
    WriteOutcome::Written
  );
  assert_eq!(
    client.write_action_if_absent("project", &result).await.unwrap(),
    WriteOutcome::AlreadyPresent
  );
  assert_eq!(
    client
      .get_action("project", &result.action)
      .await
      .unwrap()
      .unwrap()
      .result,
    result
  );
  let mut restored = Vec::new();
  server.fail_next(1);
  client
    .read_blob(&blob)
    .await
    .unwrap()
    .read_to_end(&mut restored)
    .await
    .unwrap();
  assert_eq!(restored, bytes);
}

#[tokio::test]
async fn separate_local_caches_share_one_remote_result() {
  let server = ReferenceServer::start().await;
  let first_root = TempDir::new().unwrap();
  let second_root = TempDir::new().unwrap();
  let first_local = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(first_root.path())).unwrap());
  let second_local = Arc::new(LocalCacheStore::open(LocalCacheConfig::new(second_root.path())).unwrap());
  let first = LayeredCacheStore::new(first_local, Arc::new(client(&server)));
  let second = LayeredCacheStore::new(second_local.clone(), Arc::new(client(&server)));
  let (blob, bytes, result) = fixture();

  assert_eq!(
    first
      .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes)))
      .await
      .unwrap(),
    WriteOutcome::Written
  );
  assert_eq!(
    first.write_action_if_absent("shared", &result).await.unwrap(),
    WriteOutcome::Written
  );
  let lookup = second.get_action("shared", &result.action).await.unwrap().unwrap();
  assert_eq!(lookup.result, result);
  assert_eq!(lookup.layer, octa_cache_protocol::CacheLayer::Remote);
  // Metadata lookup is intentionally lazy: job-specific bundle limits are
  // checked by the executor before this download is allowed to start.
  assert!(second_local
    .get_action("shared", &result.action)
    .await
    .unwrap()
    .is_none());
  let mut restored = Vec::new();
  second
    .read_blob(&blob)
    .await
    .unwrap()
    .read_to_end(&mut restored)
    .await
    .unwrap();
  assert_eq!(restored, b"portable-result");
  second.write_action_if_absent("shared", &result).await.unwrap();
  assert!(second_local
    .get_action("shared", &result.action)
    .await
    .unwrap()
    .is_some());
  assert!(second_local
    .find_missing_blobs(std::slice::from_ref(&blob))
    .await
    .unwrap()
    .is_empty());
  assert!(server.request_count() >= 4);
}

#[test]
fn token_files_are_bounded_private_and_redacted() {
  let directory = TempDir::new().unwrap();
  let path = directory.path().join("token");
  fs::write(&path, "top-secret\n").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
  }
  let config = HttpCacheConfig::from_token_file("https://cache.example", &path).unwrap();
  assert!(!format!("{config:?}").contains("top-secret"));
  fs::write(&path, []).unwrap();
  assert!(HttpCacheConfig::from_token_file("https://cache.example", &path).is_err());

  fs::write(&path, [0xff]).unwrap();
  assert!(HttpCacheConfig::from_token_file("https://cache.example", &path).is_err());
  fs::write(&path, "has whitespace").unwrap();
  assert!(HttpCacheConfig::from_token_file("https://cache.example", &path).is_err());
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt as _;
    fs::write(&path, "secret").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(HttpCacheConfig::from_token_file("https://cache.example", &path).is_err());
  }
  fs::remove_file(&path).unwrap();
  fs::create_dir(&path).unwrap();
  assert!(HttpCacheConfig::from_token_file("https://cache.example", &path).is_err());
  fs::remove_dir(&path).unwrap();
  assert!(HttpCacheConfig::from_token_file("https://cache.example", &path).is_err());
}

#[test]
fn custom_ca_files_are_bounded_and_never_disable_tls_validation() {
  let directory = TempDir::new().unwrap();
  let path = directory.path().join("ca.pem");
  assert!(HttpCacheConfig::loopback("http://localhost:1234")
    .with_ca_certificate_file(Some(&path))
    .is_err());
  fs::write(&path, "not a certificate").unwrap();
  let config = HttpCacheConfig::loopback("http://localhost:1234");
  assert!(config.clone().with_ca_certificate_file(Some(&path)).is_err());
  assert!(config.with_ca_certificate_file(None).is_ok());
  fs::write(&path, vec![b'x'; 1024 * 1024 + 1]).unwrap();
  assert!(HttpCacheConfig::loopback("http://localhost:1234")
    .with_ca_certificate_file(Some(&path))
    .is_err());
}

#[test]
fn rejects_unsafe_endpoints_and_invalid_operational_bounds() {
  for endpoint in [
    "http://cache.example",
    "https://user:password@cache.example",
    "https://cache.example?token=value",
    "https://cache.example#fragment",
    "not a URL",
  ] {
    assert!(crate::config::HttpCacheConfig::new_for_test(endpoint).is_err());
  }
  let oversized = format!(
    "https://cache.example/{}",
    "x".repeat(octa_cache_protocol::MAX_CACHE_STRING_BYTES)
  );
  assert!(crate::config::HttpCacheConfig::new_for_test(&oversized).is_err());
  let mut config = HttpCacheConfig::loopback("http://localhost:1234/prefix");
  assert!(format!("{config:?}").contains("/prefix/"));
  config.policy.request_timeout = std::time::Duration::ZERO;
  assert!(config.validate().is_err());
  config.policy.request_timeout = std::time::Duration::from_secs(1);
  config.policy.max_parallel_transfers = 0;
  assert!(config.validate().is_err());
  config.policy.max_parallel_transfers = 1;
  config.policy.retry_base_delay = std::time::Duration::from_secs(2);
  config.policy.retry_max_delay = std::time::Duration::from_secs(1);
  assert!(config.validate().is_err());
  config.policy.retry_base_delay = std::time::Duration::ZERO;
  config.policy.retry_max_delay = std::time::Duration::from_millis(1);
  config.policy.request_timeout = crate::MAX_REQUEST_TIMEOUT + Duration::from_secs(1);
  assert!(config.validate().is_err());
  config.policy.request_timeout = Duration::from_secs(1);
  config.policy.max_parallel_transfers = crate::MAX_PARALLEL_TRANSFERS + 1;
  assert!(config.validate().is_err());
  config.policy.max_parallel_transfers = 1;
  config.policy.max_retries = crate::MAX_RETRIES + 1;
  assert!(config.validate().is_err());
  config.policy.max_retries = 0;
  config.policy.circuit_open_duration = crate::MAX_CIRCUIT_OPEN_DURATION + Duration::from_secs(1);
  assert!(config.validate().is_err());
}

#[tokio::test]
async fn one_deadline_bounds_the_complete_operation() {
  let server = TestServer::delayed(Duration::from_millis(200)).await;
  let mut config = HttpCacheConfig::loopback(&server.endpoint());
  config.policy.request_timeout = Duration::from_millis(30);
  config.policy.max_retries = 3;
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  let started = tokio::time::Instant::now();
  let error = client
    .get_action("project", &Digest::blake3(b"deadline"))
    .await
    .unwrap_err();
  assert!(error.to_string().contains("whole-operation deadline"));
  assert!(started.elapsed() < Duration::from_millis(150));

  // The same deadline starts before upload staging. A caller that never
  // finishes producing its stream cannot occupy a transfer slot forever.
  let reference = ReferenceServer::start().await;
  let mut config = HttpCacheConfig::loopback(&reference.endpoint());
  config.policy.request_timeout = Duration::from_millis(30);
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  let (mut writer, reader) = tokio::io::duplex(16);
  tokio::spawn(async move {
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _ = writer.write_all(b"portable-result").await;
  });
  let (blob, _, _) = fixture();
  let error = client.write_blob_if_absent(&blob, Box::pin(reader)).await.unwrap_err();
  assert!(error.to_string().contains("whole-operation deadline"));
}

#[tokio::test]
async fn retries_interrupted_metadata_and_blob_bodies() {
  let (_, _, result) = fixture();
  let metadata = serde_json::to_vec(&result).unwrap();
  let (metadata_server, metadata_requests) = TestServer::flaky_body(metadata, REMOTE_CACHE_JSON_CONTENT_TYPE).await;
  let mut config = HttpCacheConfig::loopback(&metadata_server.endpoint());
  config.policy.max_retries = 1;
  config.policy.retry_base_delay = Duration::ZERO;
  config.policy.retry_max_delay = Duration::from_millis(1);
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  assert!(client.get_action("project", &result.action).await.unwrap().is_some());
  assert_eq!(metadata_requests.load(Ordering::Relaxed), 2);

  let (blob, bytes, _) = fixture();
  let (blob_server, blob_requests) = TestServer::flaky_body(bytes.clone(), REMOTE_CACHE_BLOB_CONTENT_TYPE).await;
  let mut config = HttpCacheConfig::loopback(&blob_server.endpoint());
  config.policy.max_retries = 1;
  config.policy.retry_base_delay = Duration::ZERO;
  config.policy.retry_max_delay = Duration::from_millis(1);
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  let mut restored = Vec::new();
  client
    .read_blob(&blob)
    .await
    .unwrap()
    .read_to_end(&mut restored)
    .await
    .unwrap();
  assert_eq!(restored, bytes);
  assert_eq!(blob_requests.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn retries_server_hints_and_opens_a_run_scoped_circuit() {
  let server = ReferenceServer::start().await;
  server.fail_next(1);
  let client = client(&server);
  assert!(client
    .get_action("project", &Digest::blake3(b"missing"))
    .await
    .unwrap()
    .is_none());
  assert_eq!(server.request_count(), 2);

  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let endpoint = format!("http://{}/", listener.local_addr().unwrap());
  drop(listener);
  let mut config = HttpCacheConfig::loopback(&endpoint);
  config.policy.max_retries = 0;
  config.policy.circuit_failure_threshold = 1;
  let unavailable = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  assert!(format!("{unavailable:?}").contains("HttpCacheStore"));
  assert!(unavailable
    .get_action("project", &Digest::blake3(b"first"))
    .await
    .is_err());
  let error = unavailable
    .get_action("project", &Digest::blake3(b"second"))
    .await
    .unwrap_err();
  assert!(error.to_string().contains("circuit is open"));
}

#[tokio::test]
async fn retries_network_failures_with_client_backoff() {
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let endpoint = format!("http://{}/", listener.local_addr().unwrap());
  drop(listener);
  let mut config = HttpCacheConfig::loopback(&endpoint);
  config.policy.max_retries = 1;
  config.policy.retry_base_delay = std::time::Duration::ZERO;
  config.policy.retry_max_delay = std::time::Duration::from_millis(1);
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  assert!(client.get_action("project", &Digest::blake3(b"retry")).await.is_err());
}

#[tokio::test]
async fn write_and_blob_operations_bound_retries_failures_deadlines_and_cancellation() {
  let (_, _, mut result) = fixture();
  result.output_bundle = None;

  let reference = ReferenceServer::start().await;
  reference.fail_next(1);
  let mut retry_config = HttpCacheConfig::loopback(&reference.endpoint());
  retry_config.policy.max_retries = 1;
  retry_config.policy.retry_base_delay = Duration::ZERO;
  retry_config.policy.retry_max_delay = Duration::from_millis(1);
  let retrying = HttpCacheStore::new(retry_config, CancellationToken::new()).unwrap();
  assert_eq!(
    retrying.write_action_if_absent("project", &result).await.unwrap(),
    WriteOutcome::Written
  );

  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let endpoint = format!("http://{}/", listener.local_addr().unwrap());
  drop(listener);
  let mut unavailable_config = HttpCacheConfig::loopback(&endpoint);
  unavailable_config.policy.max_retries = 1;
  unavailable_config.policy.retry_base_delay = Duration::ZERO;
  unavailable_config.policy.retry_max_delay = Duration::from_millis(1);
  let unavailable = HttpCacheStore::new(unavailable_config, CancellationToken::new()).unwrap();
  assert!(unavailable.write_action_if_absent("project", &result).await.is_err());
  assert!(unavailable.read_blob(&fixture().0).await.is_err());

  let delayed = TestServer::delayed(Duration::from_millis(100)).await;
  let mut timeout_config = HttpCacheConfig::loopback(&delayed.endpoint());
  timeout_config.policy.request_timeout = Duration::from_millis(10);
  timeout_config.policy.max_retries = 0;
  let timeout = HttpCacheStore::new(timeout_config, CancellationToken::new()).unwrap();
  assert!(timeout.write_action_if_absent("project", &result).await.is_err());
  assert!(timeout.read_blob(&fixture().0).await.is_err());

  let cancelled = CancellationToken::new();
  cancelled.cancel();
  let cancelled = HttpCacheStore::new(HttpCacheConfig::loopback(&reference.endpoint()), cancelled).unwrap();
  assert!(matches!(
    cancelled.write_action_if_absent("project", &result).await,
    Err(octa_cache::CacheError::Cancelled)
  ));
  assert!(matches!(
    cancelled.read_blob(&fixture().0).await,
    Err(octa_cache::CacheError::Cancelled)
  ));
}

#[tokio::test]
async fn terminal_body_failures_and_retry_hints_preserve_the_operation_deadline() {
  let (_, _, result) = fixture();
  let metadata = TestServer::broken_body(serde_json::to_vec(&result).unwrap(), REMOTE_CACHE_JSON_CONTENT_TYPE).await;
  let mut config = HttpCacheConfig::loopback(&metadata.endpoint());
  config.policy.max_retries = 0;
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  assert!(client.get_action("project", &result.action).await.is_err());

  let (blob, bytes, _) = fixture();
  let body = TestServer::broken_body(bytes, REMOTE_CACHE_BLOB_CONTENT_TYPE).await;
  let mut config = HttpCacheConfig::loopback(&body.endpoint());
  config.policy.max_retries = 0;
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  assert!(client.read_blob(&blob).await.is_err());

  let retry = TestServer::retry_after(1).await;
  let mut config = HttpCacheConfig::loopback(&retry.endpoint());
  config.policy.request_timeout = Duration::from_millis(10);
  config.policy.max_retries = 1;
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  let error = client
    .get_action("project", &Digest::blake3(b"retry-deadline"))
    .await
    .unwrap_err();
  assert!(error.to_string().contains("whole-operation deadline"));
}

#[tokio::test]
async fn rejects_bad_authentication_and_preserves_cancellation() {
  let server = ReferenceServer::start().await;
  let mut config = HttpCacheConfig::loopback(&server.endpoint());
  config.token = crate::config::BearerToken::fixture("wrong-token");
  let client = HttpCacheStore::new(config, CancellationToken::new()).unwrap();
  let error = client
    .get_action("project", &Digest::blake3(b"unauthorized"))
    .await
    .unwrap_err();
  assert!(error.to_string().contains("401"));

  let cancellation = CancellationToken::new();
  cancellation.cancel();
  let client = HttpCacheStore::new(HttpCacheConfig::loopback(&server.endpoint()), cancellation).unwrap();
  assert!(matches!(
    client.get_action("project", &Digest::blake3(b"cancelled")).await,
    Err(octa_cache::CacheError::Cancelled)
  ));
}

#[tokio::test]
async fn rejects_untrusted_metadata_status_and_blob_lengths() {
  let action = Digest::blake3(b"action");
  let no_protocol = StaticServer::start(StatusCode::OK, b"{}".to_vec(), false).await;
  assert!(static_client(&no_protocol)
    .get_action("project", &action)
    .await
    .is_err());

  let invalid_json = StaticServer::start(StatusCode::OK, b"not-json".to_vec(), true).await;
  assert!(static_client(&invalid_json)
    .get_action("project", &action)
    .await
    .is_err());

  let wrong_content_type = TestServer::start(
    Router::new().fallback(|| async { typed_response(StatusCode::OK, "text/html", "<html></html>") }),
  )
  .await;
  let client = HttpCacheStore::new(
    HttpCacheConfig::loopback(&wrong_content_type.endpoint()),
    CancellationToken::new(),
  )
  .unwrap();
  assert!(client.get_action("project", &action).await.is_err());

  let mut invalid_result = fixture().2;
  invalid_result.result_version = 2;
  let invalid_result = StaticServer::start(StatusCode::OK, serde_json::to_vec(&invalid_result).unwrap(), true).await;
  assert!(static_client(&invalid_result)
    .get_action("project", &action)
    .await
    .is_err());

  let mut misbound = fixture().2;
  misbound.action = Digest::blake3(b"another-action");
  let misbound = StaticServer::start(StatusCode::OK, serde_json::to_vec(&misbound).unwrap(), true).await;
  assert!(static_client(&misbound).get_action("project", &action).await.is_err());

  let oversized = StaticServer::start(
    StatusCode::OK,
    vec![b'x'; octa_cache_protocol::MAX_ACTION_RESULT_WIRE_BYTES + 1],
    true,
  )
  .await;
  assert!(static_client(&oversized).get_action("project", &action).await.is_err());

  let denied = StaticServer::start(StatusCode::FORBIDDEN, Vec::new(), true).await;
  assert!(static_client(&denied).get_action("project", &action).await.is_err());
  assert!(static_client(&denied).find_missing_blobs(&[]).await.is_err());

  let (blob, _, _) = fixture();
  assert!(static_client(&denied).read_blob(&blob).await.is_err());
  let wrong_length = TestServer::start(
    Router::new().fallback(|| async { typed_response(StatusCode::OK, REMOTE_CACHE_BLOB_CONTENT_TYPE, "short") }),
  )
  .await;
  let wrong_length_client = HttpCacheStore::new(
    HttpCacheConfig::loopback(&wrong_length.endpoint()),
    CancellationToken::new(),
  )
  .unwrap();
  assert!(wrong_length_client.read_blob(&blob).await.is_err());
  let truncated = StaticServer::streaming(
    vec![vec![b'x'; blob.encoded_size_bytes as usize - 1]],
    REMOTE_CACHE_BLOB_CONTENT_TYPE,
  )
  .await;
  assert!(static_client(&truncated).read_blob(&blob).await.is_err());
  let overflowing = StaticServer::streaming(
    vec![vec![b'x'; blob.encoded_size_bytes as usize + 1]],
    REMOTE_CACHE_BLOB_CONTENT_TYPE,
  )
  .await;
  assert!(static_client(&overflowing).read_blob(&blob).await.is_err());

  let invalid_missing = StaticServer::start(StatusCode::OK, b"not-json".to_vec(), true).await;
  assert!(static_client(&invalid_missing).find_missing_blobs(&[]).await.is_err());

  let oversized_stream = StaticServer::streaming(
    vec![vec![b'x'; 1024 * 1024], vec![b'x'; 1024 * 1024 + 1]],
    REMOTE_CACHE_JSON_CONTENT_TYPE,
  )
  .await;
  assert!(static_client(&oversized_stream).find_missing_blobs(&[]).await.is_err());

  let invalid_missing = FindMissingBlobsResponseV1 {
    protocol_version: 2,
    missing: Vec::new(),
  };
  let invalid_missing = StaticServer::start(StatusCode::OK, serde_json::to_vec(&invalid_missing).unwrap(), true).await;
  assert!(static_client(&invalid_missing).find_missing_blobs(&[]).await.is_err());

  let invalid_missing = FindMissingBlobsResponseV1 {
    protocol_version: REMOTE_CACHE_PROTOCOL_V1,
    missing: vec![blob],
  };
  let invalid_missing = StaticServer::start(StatusCode::OK, serde_json::to_vec(&invalid_missing).unwrap(), true).await;
  assert!(static_client(&invalid_missing).find_missing_blobs(&[]).await.is_err());
}

#[tokio::test]
async fn reports_conflicts_ambiguous_preconditions_and_invalid_upload_lengths() {
  let server = ReferenceServer::start().await;
  let client = client(&server);
  let (blob, bytes, result) = fixture();
  let compressed = BlobDescriptor {
    encoding: BlobEncoding::ZstdV1,
    ..blob.clone()
  };
  client
    .write_blob_if_absent(&compressed, Box::pin(std::io::Cursor::new(bytes.clone())))
    .await
    .unwrap();
  let mut conflicting = bytes.clone();
  conflicting[0] ^= 1;
  assert_eq!(
    client
      .write_blob_if_absent(&compressed, Box::pin(std::io::Cursor::new(conflicting)))
      .await
      .unwrap(),
    WriteOutcome::Conflict
  );
  client
    .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(bytes)))
    .await
    .unwrap();
  client.write_action_if_absent("project", &result).await.unwrap();
  let mut conflicting = result.clone();
  conflicting.stdout = Some("different".to_owned());
  assert_eq!(
    client.write_action_if_absent("project", &conflicting).await.unwrap(),
    WriteOutcome::Conflict
  );
  assert!(client
    .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(Vec::new())))
    .await
    .is_err());
  assert!(client
    .write_blob_if_absent(
      &blob,
      Box::pin(std::io::Cursor::new(vec![0; blob.encoded_size_bytes as usize + 1])),
    )
    .await
    .is_err());

  let precondition = StaticServer::start(StatusCode::PRECONDITION_FAILED, Vec::new(), true).await;
  assert!(static_client(&precondition)
    .write_action_if_absent("project", &result)
    .await
    .is_err());
  assert!(static_client(&precondition)
    .write_blob_if_absent(
      &blob,
      Box::pin(std::io::Cursor::new(vec![0; blob.encoded_size_bytes as usize])),
    )
    .await
    .is_err());

  let forbidden = StaticServer::start(StatusCode::FORBIDDEN, Vec::new(), true).await;
  assert!(static_client(&forbidden)
    .write_action_if_absent("project", &result)
    .await
    .is_err());
}

#[tokio::test]
async fn preserves_zstd_physical_identity_in_blob_routes() {
  let server = ReferenceServer::start().await;
  let client = client(&server);
  let encoded = b"encoded-zstd".to_vec();
  let expanded = b"expanded-zstd-value";
  let blob = BlobDescriptor {
    digest: Digest::blake3(expanded),
    encoding: BlobEncoding::ZstdV1,
    encoded_size_bytes: encoded.len() as u64,
    expanded_size_bytes: expanded.len() as u64,
    entry_count: 1,
  };
  assert_eq!(
    client
      .write_blob_if_absent(&blob, Box::pin(std::io::Cursor::new(encoded.clone())))
      .await
      .unwrap(),
    WriteOutcome::Written
  );
  let mut restored = Vec::new();
  client
    .read_blob(&blob)
    .await
    .unwrap()
    .read_to_end(&mut restored)
    .await
    .unwrap();
  assert_eq!(restored, encoded);
}
