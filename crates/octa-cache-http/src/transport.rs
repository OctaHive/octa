//! Small HTTP primitives shared by metadata and blob routes.
//!
//! Route construction and cache semantics stay in `client`; this module owns
//! only transport classification and bounded response consumption.

use std::time::Duration;

use futures::StreamExt as _;
use octa_cache::CacheError;
use reqwest::{header, Response, StatusCode};

pub(crate) fn is_retryable_request_error(error: &reqwest::Error) -> bool {
  error.is_timeout() || error.is_connect() || error.is_request() || error.is_body()
}

/// Formats the error chain without request headers or bodies. Reqwest's
/// top-level display often omits the TLS or socket cause needed by operators.
pub(crate) fn request_error_message(error: &reqwest::Error) -> String {
  use std::error::Error as _;

  let mut message = error.to_string();
  let mut source = error.source();
  while let Some(error) = source {
    message.push_str(": ");
    message.push_str(&error.to_string());
    source = error.source();
  }
  message
}

pub(crate) fn is_retryable_status(status: StatusCode) -> bool {
  matches!(
    status,
    StatusCode::REQUEST_TIMEOUT
      | StatusCode::TOO_EARLY
      | StatusCode::TOO_MANY_REQUESTS
      | StatusCode::INTERNAL_SERVER_ERROR
      | StatusCode::BAD_GATEWAY
      | StatusCode::SERVICE_UNAVAILABLE
      | StatusCode::GATEWAY_TIMEOUT
  )
}

pub(crate) fn retry_after(response: &Response) -> Option<Duration> {
  response
    .headers()
    .get(header::RETRY_AFTER)?
    .to_str()
    .ok()?
    .parse::<u64>()
    .ok()
    .map(Duration::from_secs)
}

#[derive(Debug)]
pub(crate) enum BoundedReadError {
  Transport(reqwest::Error),
  Limit,
}

/// Consumes metadata with a hard bound even when `Content-Length` is absent or false.
pub(crate) async fn read_bounded(response: Response, limit: usize) -> Result<Vec<u8>, BoundedReadError> {
  if response.content_length().is_some_and(|length| length > limit as u64) {
    return Err(BoundedReadError::Limit);
  }
  let mut stream = response.bytes_stream();
  let mut bytes = Vec::new();
  while let Some(chunk) = stream.next().await {
    let chunk = chunk.map_err(BoundedReadError::Transport)?;
    if bytes.len().saturating_add(chunk.len()) > limit {
      return Err(BoundedReadError::Limit);
    }
    bytes.extend_from_slice(&chunk);
  }
  Ok(bytes)
}

pub(crate) fn remote_error(operation: &'static str, message: impl Into<String>) -> CacheError {
  CacheError::Remote {
    operation,
    message: message.into(),
  }
}
