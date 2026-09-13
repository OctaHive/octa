//! `CacheStore` adapter for the version-one HTTP routes.

use std::{future::Future, sync::Arc, time::Duration};

use async_trait::async_trait;
use futures::StreamExt as _;
use octa_cache::{ActionLookup, BlobReader, CacheError, CacheResult, CacheStore, WriteOutcome};
use octa_cache_protocol::{
  validate_namespace, ActionResultV1, BlobDescriptor, BlobEncoding, CacheLayer, Digest, FindMissingBlobsRequestV1,
  FindMissingBlobsResponseV1, WriteActionRequestV1, MAX_ACTION_RESULT_WIRE_BYTES, MAX_REMOTE_CACHE_METADATA_BYTES,
  REMOTE_CACHE_BLOB_CONTENT_TYPE, REMOTE_CACHE_JSON_CONTENT_TYPE, REMOTE_CACHE_PROTOCOL_HEADER,
  REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1, REMOTE_CACHE_PROTOCOL_V1,
};
use reqwest::{header, RequestBuilder, Response, StatusCode, Url};
use tokio::{
  io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _},
  sync::Semaphore,
  time::Instant,
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};

use crate::{
  circuit::{CircuitBreaker, CircuitPermit},
  config::{CONNECTION_POOL_IDLE_TIMEOUT, TCP_KEEPALIVE_INTERVAL},
  transport::{
    is_retryable_request_error, is_retryable_status, read_bounded, remote_error, request_error_message, retry_after,
    BoundedReadError,
  },
  HttpCacheConfig, HttpCacheError,
};

/// Remote implementation of Octa's transport-independent cache boundary.
///
/// One value is created per CLI run or runner job. Its `reqwest::Client`
/// supplies persistent connection pooling, while its circuit breaker never
/// leaks failure state into a later independent run.
#[derive(Clone)]
pub struct HttpCacheStore {
  inner: Arc<Inner>,
}

struct Inner {
  config: HttpCacheConfig,
  client: reqwest::Client,
  transfers: Semaphore,
  circuit: CircuitBreaker,
  cancel: CancellationToken,
}

#[derive(Clone, Copy)]
struct Operation {
  deadline: Instant,
  circuit: CircuitPermit,
}

/// Result of consuming one HTTP attempt.
///
/// A response-body transport failure is retryable only while the response is
/// still private to the adapter. Completed metadata or a verified staging file
/// is returned with `Complete` and can no longer be replayed accidentally.
enum Attempt<T> {
  Complete(T),
  Retry(CacheError),
}

impl std::fmt::Debug for HttpCacheStore {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("HttpCacheStore")
      .field("config", &self.inner.config)
      .finish_non_exhaustive()
  }
}

impl HttpCacheStore {
  /// Creates one run-scoped client using `cancel` for all in-flight operations.
  pub fn new(config: HttpCacheConfig, cancel: CancellationToken) -> Result<Self, HttpCacheError> {
    config.validate()?;
    let mut builder = reqwest::Client::builder()
      .pool_idle_timeout(CONNECTION_POOL_IDLE_TIMEOUT)
      .tcp_keepalive(TCP_KEEPALIVE_INTERVAL)
      .redirect(reqwest::redirect::Policy::none());
    if let Some(certificate) = &config.root_certificate {
      builder = builder.add_root_certificate(certificate.clone());
    }
    let client = builder.build()?;
    Ok(Self {
      inner: Arc::new(Inner {
        transfers: Semaphore::new(config.policy.max_parallel_transfers),
        circuit: CircuitBreaker::new(
          config.policy.circuit_failure_threshold,
          config.policy.circuit_open_duration,
        ),
        config,
        client,
        cancel,
      }),
    })
  }

  fn action_url(&self, action: &Digest, namespace: &str) -> Result<Url, HttpCacheError> {
    validate_namespace(namespace)?;
    let mut url = self.inner.config.endpoint.join("v1/actions")?;
    url
      .path_segments_mut()
      .map_err(|_| HttpCacheError::Configuration("remote cache endpoint cannot contain path segments".to_owned()))?
      .extend(&[
        &action.algorithm().to_string(),
        &action.hex(),
        &action.size_bytes().to_string(),
      ]);
    url.query_pairs_mut().append_pair("namespace", namespace);
    Ok(url)
  }

  fn blob_url(&self, blob: &BlobDescriptor) -> Result<Url, HttpCacheError> {
    blob.validate()?;
    let mut url = self.inner.config.endpoint.join("v1/blobs")?;
    let encoding = match blob.encoding {
      BlobEncoding::Identity => "identity",
      BlobEncoding::ZstdV1 => "zstd_v1",
    };
    url
      .path_segments_mut()
      .map_err(|_| HttpCacheError::Configuration("remote cache endpoint cannot contain path segments".to_owned()))?
      .extend(&[
        &blob.digest.algorithm().to_string(),
        &blob.digest.hex(),
        &blob.expanded_size_bytes.to_string(),
        encoding,
        &blob.encoded_size_bytes.to_string(),
        &blob.entry_count.to_string(),
      ]);
    Ok(url)
  }

  fn request(&self, builder: RequestBuilder) -> RequestBuilder {
    builder
      .bearer_auth(self.inner.config.token.value())
      .header(REMOTE_CACHE_PROTOCOL_HEADER, REMOTE_CACHE_PROTOCOL_V1)
  }

  fn start_operation(&self) -> CacheResult<Operation> {
    let circuit = self.inner.circuit.permit().map_err(CacheError::from)?;
    let deadline = Instant::now()
      .checked_add(self.inner.config.policy.request_timeout)
      .ok_or_else(|| remote_error("start operation", "operation deadline overflow"))?;
    Ok(Operation { deadline, circuit })
  }

  /// Applies the single deadline, cancellation token, and circuit transition
  /// to the complete operation, including semaphore waits and staging I/O.
  async fn run_operation<T, F>(&self, state: Operation, operation: &'static str, future: F) -> CacheResult<T>
  where
    F: Future<Output = CacheResult<T>>,
  {
    let result = tokio::select! {
      _ = self.inner.cancel.cancelled() => Err(CacheError::Cancelled),
      _ = tokio::time::sleep_until(state.deadline) => Err(operation_timeout(operation)),
      result = future => result,
    };
    match &result {
      Ok(_) => self.inner.circuit.success(state.circuit),
      Err(CacheError::Cancelled) => {},
      Err(CacheError::Remote { .. }) => self.inner.circuit.failure(state.circuit),
      Err(_) => {},
    }
    result
  }

  /// Drives every HTTP operation through the same bounded retry policy.
  ///
  /// `consume` may request another attempt only for a retryable failure while
  /// consuming a private response body. Protocol and semantic failures return
  /// directly and are never hidden by retries.
  async fn request_with_retry<T, B, C, F>(
    &self,
    operation: &'static str,
    mut build: B,
    mut consume: C,
  ) -> CacheResult<T>
  where
    B: FnMut() -> CacheResult<RequestBuilder>,
    C: FnMut(Response) -> F,
    F: Future<Output = CacheResult<Attempt<T>>>,
  {
    let attempts = usize::from(self.inner.config.policy.max_retries) + 1;
    for attempt in 0..attempts {
      let response = match self.request(build()?).send().await {
        Ok(response) => response,
        Err(error) if attempt + 1 < attempts && is_retryable_request_error(&error) => {
          self.wait_retry(self.backoff(attempt)).await;
          continue;
        },
        Err(error) => return Err(remote_error(operation, request_error_message(&error))),
      };
      if is_retryable_status(response.status()) && attempt + 1 < attempts {
        let delay = retry_after(&response).unwrap_or_else(|| self.backoff(attempt));
        drop(response);
        self.wait_retry(delay).await;
        continue;
      }
      match consume(response).await? {
        Attempt::Complete(result) => return Ok(result),
        Attempt::Retry(_error) if attempt + 1 < attempts => self.wait_retry(self.backoff(attempt)).await,
        Attempt::Retry(error) => return Err(error),
      }
    }
    unreachable!("retry loop always returns on its final attempt")
  }

  async fn wait_retry(&self, delay: Duration) {
    tokio::time::sleep(delay.min(self.inner.config.policy.retry_max_delay)).await;
  }

  fn backoff(&self, attempt: usize) -> Duration {
    let factor = 1_u32.checked_shl(attempt.min(31) as u32).unwrap_or(u32::MAX);
    let ceiling = self
      .inner
      .config
      .policy
      .retry_base_delay
      .saturating_mul(factor)
      .min(self.inner.config.policy.retry_max_delay);
    if ceiling.is_zero() {
      ceiling
    } else {
      Duration::from_millis(fastrand::u64(0..=ceiling.as_millis().min(u128::from(u64::MAX)) as u64))
    }
  }

  fn accept_response(operation: &'static str, response: &Response) -> CacheResult<()> {
    let version = response
      .headers()
      .get(REMOTE_CACHE_PROTOCOL_HEADER)
      .and_then(|value| value.to_str().ok());
    if version != Some(REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1) {
      return Err(remote_error(
        operation,
        format!(
          "HTTP {} response from '{}' omitted the supported protocol version",
          response.status(),
          response.url()
        ),
      ));
    }
    Ok(())
  }

  fn accept_content_type(operation: &'static str, response: &Response, expected: &str) -> CacheResult<()> {
    let actual = response
      .headers()
      .get(header::CONTENT_TYPE)
      .and_then(|value| value.to_str().ok())
      .and_then(|value| value.split(';').next())
      .map(str::trim);
    if !actual.is_some_and(|actual| actual.eq_ignore_ascii_case(expected)) {
      return Err(remote_error(
        operation,
        format!("HTTP {} response has an unsupported content type", response.status()),
      ));
    }
    Ok(())
  }

  fn status_error(operation: &'static str, response: Response) -> CacheError {
    let status = response.status();
    drop(response);
    // Response bodies are controlled by the remote service and may echo
    // sensitive request context. Status is sufficient and safe to log.
    remote_error(operation, format!("server returned HTTP {status}"))
  }

  async fn consume_metadata(
    response: Response,
    operation: &'static str,
    limit: usize,
    accepted: &[StatusCode],
  ) -> CacheResult<Attempt<(StatusCode, Vec<u8>)>> {
    Self::accept_response(operation, &response)?;
    let status = response.status();
    if !accepted.contains(&status) {
      return Err(Self::status_error(operation, response));
    }
    if status == StatusCode::NOT_FOUND {
      return Ok(Attempt::Complete((status, Vec::new())));
    }
    Self::accept_content_type(operation, &response, REMOTE_CACHE_JSON_CONTENT_TYPE)?;
    match read_bounded(response, limit).await {
      Ok(bytes) => Ok(Attempt::Complete((status, bytes))),
      Err(BoundedReadError::Transport(error)) if is_retryable_request_error(&error) => {
        Ok(Attempt::Retry(remote_error(operation, request_error_message(&error))))
      },
      Err(BoundedReadError::Transport(error)) => Err(remote_error(operation, request_error_message(&error))),
      Err(BoundedReadError::Limit) => Err(remote_error(operation, format!("response exceeds {limit} bytes"))),
    }
  }

  async fn consume_blob(response: Response, blob: &BlobDescriptor) -> CacheResult<Attempt<BlobReader>> {
    const OPERATION: &str = "read blob";
    Self::accept_response(OPERATION, &response)?;
    if response.status() != StatusCode::OK {
      return Err(Self::status_error(OPERATION, response));
    }
    Self::accept_content_type(OPERATION, &response, REMOTE_CACHE_BLOB_CONTENT_TYPE)?;
    if response
      .content_length()
      .is_some_and(|length| length != blob.encoded_size_bytes)
    {
      return Err(remote_error(OPERATION, "response length differs from blob descriptor"));
    }

    // Bytes remain private until their physical length is verified. A failed
    // response can therefore be retried without exposing a partial stream.
    let file = tempfile::tempfile().map_err(CacheError::TemporaryFile)?;
    let mut file = tokio::fs::File::from_std(file);
    let mut stream = response.bytes_stream();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
      let chunk = match chunk {
        Ok(chunk) => chunk,
        Err(error) if is_retryable_request_error(&error) => {
          return Ok(Attempt::Retry(remote_error(OPERATION, request_error_message(&error))));
        },
        Err(error) => return Err(remote_error(OPERATION, request_error_message(&error))),
      };
      written = written.saturating_add(chunk.len() as u64);
      if written > blob.encoded_size_bytes {
        return Err(remote_error(OPERATION, "response exceeds blob descriptor"));
      }
      file.write_all(&chunk).await.map_err(CacheError::TemporaryFile)?;
    }
    if written != blob.encoded_size_bytes {
      return Ok(Attempt::Retry(remote_error(
        OPERATION,
        "response ended before the declared blob length",
      )));
    }
    file.flush().await.map_err(CacheError::TemporaryFile)?;
    file.rewind().await.map_err(CacheError::TemporaryFile)?;
    Ok(Attempt::Complete(Box::pin(file)))
  }

  fn consume_write(response: Response, operation: &'static str) -> CacheResult<Attempt<WriteOutcome>> {
    Self::accept_response(operation, &response)?;
    match write_outcome(response.status()) {
      Some(outcome) => Ok(Attempt::Complete(outcome)),
      None => Err(Self::status_error(operation, response)),
    }
  }
}

#[async_trait]
impl CacheStore for HttpCacheStore {
  async fn get_action(&self, namespace: &str, action: &Digest) -> CacheResult<Option<ActionLookup>> {
    let state = self.start_operation()?;
    let url = self.action_url(action, namespace).map_err(CacheError::from)?;
    self
      .run_operation(state, "get action", async {
        let (status, bytes) = self
          .request_with_retry(
            "get action",
            || {
              Ok(
                self
                  .inner
                  .client
                  .get(url.clone())
                  .header(header::ACCEPT, REMOTE_CACHE_JSON_CONTENT_TYPE),
              )
            },
            |response| {
              Self::consume_metadata(
                response,
                "get action",
                MAX_ACTION_RESULT_WIRE_BYTES,
                &[StatusCode::OK, StatusCode::NOT_FOUND],
              )
            },
          )
          .await?;
        if status == StatusCode::NOT_FOUND {
          return Ok(None);
        }
        let result: ActionResultV1 = serde_json::from_slice(&bytes)
          .map_err(|error| remote_error("get action", format!("invalid action metadata: {error}")))?;
        result
          .validate()
          .map_err(|error| remote_error("get action", error.to_string()))?;
        if result.action != *action {
          return Err(remote_error("get action", "response is bound to another action digest"));
        }
        Ok(Some(ActionLookup {
          result,
          layer: CacheLayer::Remote,
        }))
      })
      .await
  }

  async fn find_missing_blobs(&self, blobs: &[BlobDescriptor]) -> CacheResult<Vec<BlobDescriptor>> {
    let state = self.start_operation()?;
    let request = FindMissingBlobsRequestV1 {
      protocol_version: REMOTE_CACHE_PROTOCOL_V1,
      blobs: blobs.to_vec(),
    };
    request.validate()?;
    let url = self
      .inner
      .config
      .endpoint
      .join("v1/blobs/missing")
      .map_err(HttpCacheError::from)?;
    self
      .run_operation(state, "find missing blobs", async {
        let (_, bytes) = self
          .request_with_retry(
            "find missing blobs",
            || {
              Ok(
                self
                  .inner
                  .client
                  .post(url.clone())
                  .header(header::ACCEPT, REMOTE_CACHE_JSON_CONTENT_TYPE)
                  .header(header::CONTENT_TYPE, REMOTE_CACHE_JSON_CONTENT_TYPE)
                  .json(&request),
              )
            },
            |response| {
              Self::consume_metadata(
                response,
                "find missing blobs",
                MAX_REMOTE_CACHE_METADATA_BYTES,
                &[StatusCode::OK],
              )
            },
          )
          .await?;
        let response: FindMissingBlobsResponseV1 = serde_json::from_slice(&bytes)
          .map_err(|error| remote_error("find missing blobs", format!("invalid response metadata: {error}")))?;
        response
          .validate()
          .map_err(|error| remote_error("find missing blobs", error.to_string()))?;
        let requested = blobs.iter().collect::<std::collections::BTreeSet<_>>();
        let missing = response.missing.iter().collect::<std::collections::BTreeSet<_>>();
        if missing.len() != response.missing.len() || !missing.is_subset(&requested) {
          return Err(remote_error(
            "find missing blobs",
            "response contains an unrequested or duplicate descriptor",
          ));
        }
        Ok(blobs.iter().filter(|blob| missing.contains(blob)).cloned().collect())
      })
      .await
  }

  async fn read_blob(&self, blob: &BlobDescriptor) -> CacheResult<BlobReader> {
    let state = self.start_operation()?;
    let url = self.blob_url(blob).map_err(CacheError::from)?;
    self
      .run_operation(state, "read blob", async {
        let _permit = self
          .inner
          .transfers
          .acquire()
          .await
          .map_err(|_| remote_error("read blob", "transfer limiter closed"))?;
        self
          .request_with_retry(
            "read blob",
            || {
              Ok(
                self
                  .inner
                  .client
                  .get(url.clone())
                  .header(header::ACCEPT, REMOTE_CACHE_BLOB_CONTENT_TYPE),
              )
            },
            |response| Self::consume_blob(response, blob),
          )
          .await
      })
      .await
  }

  async fn write_blob_if_absent(&self, blob: &BlobDescriptor, body: BlobReader) -> CacheResult<WriteOutcome> {
    blob.validate()?;
    let state = self.start_operation()?;
    let url = self.blob_url(blob).map_err(CacheError::from)?;
    self
      .run_operation(state, "write blob", async {
        let _permit = self
          .inner
          .transfers
          .acquire()
          .await
          .map_err(|_| remote_error("write blob", "transfer limiter closed"))?;
        let temporary = tempfile::NamedTempFile::new().map_err(CacheError::TemporaryFile)?;
        let mut staged = tokio::fs::File::from_std(temporary.reopen().map_err(CacheError::TemporaryFile)?);
        let copied = tokio::io::copy(&mut body.take(blob.encoded_size_bytes.saturating_add(1)), &mut staged)
          .await
          .map_err(CacheError::TemporaryFile)?;
        if copied != blob.encoded_size_bytes {
          return Err(remote_error(
            "write blob",
            "request stream length differs from blob descriptor",
          ));
        }
        staged.flush().await.map_err(CacheError::TemporaryFile)?;
        drop(staged);
        let staged = Arc::new(temporary);
        self
          .request_with_retry(
            "write blob",
            || {
              let file = staged.reopen().map_err(CacheError::TemporaryFile)?;
              Ok(
                self
                  .inner
                  .client
                  .put(url.clone())
                  .header(header::CONTENT_TYPE, REMOTE_CACHE_BLOB_CONTENT_TYPE)
                  .header(header::IF_NONE_MATCH, "*")
                  .body(reqwest::Body::wrap_stream(ReaderStream::new(
                    tokio::fs::File::from_std(file),
                  ))),
              )
            },
            |response| std::future::ready(Self::consume_write(response, "write blob")),
          )
          .await
      })
      .await
  }

  async fn write_action_if_absent(&self, namespace: &str, result: &ActionResultV1) -> CacheResult<WriteOutcome> {
    let state = self.start_operation()?;
    let request = WriteActionRequestV1 {
      protocol_version: REMOTE_CACHE_PROTOCOL_V1,
      namespace: namespace.to_owned(),
      result: result.clone(),
    };
    request.validate()?;
    let url = self.action_url(&result.action, namespace).map_err(CacheError::from)?;
    self
      .run_operation(state, "write action", async {
        self
          .request_with_retry(
            "write action",
            || {
              Ok(
                self
                  .inner
                  .client
                  .put(url.clone())
                  .header(header::CONTENT_TYPE, REMOTE_CACHE_JSON_CONTENT_TYPE)
                  .header(header::IF_NONE_MATCH, "*")
                  .json(&request),
              )
            },
            |response| std::future::ready(Self::consume_write(response, "write action")),
          )
          .await
      })
      .await
  }
}

fn write_outcome(status: StatusCode) -> Option<WriteOutcome> {
  match status {
    StatusCode::CREATED => Some(WriteOutcome::Written),
    StatusCode::NO_CONTENT => Some(WriteOutcome::AlreadyPresent),
    StatusCode::CONFLICT => Some(WriteOutcome::Conflict),
    _ => None,
  }
}

fn operation_timeout(operation: &'static str) -> CacheError {
  remote_error(operation, "whole-operation deadline exceeded")
}
