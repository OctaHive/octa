//! Strict in-memory implementation of the cache HTTP v1 contract for tests.
//!
//! The HTTP adapter tests and runner process tests intentionally share this
//! router. Keeping one reference implementation prevents an end-to-end test
//! from silently accepting a protocol shape rejected by the adapter contract.

#![warn(missing_docs)]

use std::{
  collections::HashMap,
  sync::{Arc, Mutex},
};

use axum::{
  body::Bytes,
  extract::{Path, Query, State},
  http::{header, HeaderMap, HeaderValue, StatusCode},
  response::{IntoResponse, Response},
  routing::{get, post},
  Json, Router,
};
use octa_cache_protocol::{
  ActionResultV1, BlobDescriptor, BlobEncoding, Digest, DigestAlgorithm, FindMissingBlobsRequestV1,
  FindMissingBlobsResponseV1, WriteActionRequestV1, REMOTE_CACHE_BLOB_CONTENT_TYPE, REMOTE_CACHE_JSON_CONTENT_TYPE,
  REMOTE_CACHE_PROTOCOL_HEADER, REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1, REMOTE_CACHE_PROTOCOL_V1,
};

/// Cloneable state and router factory for one isolated reference cache.
#[derive(Clone)]
pub struct ReferenceCache {
  state: Arc<Mutex<CacheData>>,
  authorization: HeaderValue,
}

#[derive(Default)]
struct CacheData {
  actions: HashMap<(String, String), ActionResultV1>,
  blobs: HashMap<String, Vec<u8>>,
  requests: usize,
  failures_remaining: usize,
}

impl ReferenceCache {
  /// Creates an empty service accepting `Bearer <token>`.
  pub fn new(token: &str) -> Self {
    Self {
      state: Arc::default(),
      authorization: HeaderValue::from_str(&format!("Bearer {token}")).expect("fixture token must be a valid header"),
    }
  }

  /// Returns the complete version-one router backed by this state.
  pub fn router(&self) -> Router {
    Router::new()
      .route(
        "/v1/actions/{algorithm}/{hash}/{size}",
        get(get_action).put(write_action),
      )
      .route("/v1/blobs/missing", post(find_missing))
      .route(
        "/v1/blobs/{algorithm}/{hash}/{expanded}/{encoding}/{encoded}/{entries}",
        get(read_blob).put(write_blob),
      )
      .with_state(self.clone())
  }

  /// Returns the number of authenticated protocol requests observed.
  pub fn request_count(&self) -> usize {
    self.state.lock().expect("reference cache mutex poisoned").requests
  }

  /// Makes the next `count` authenticated requests return retryable failures.
  pub fn fail_next(&self, count: usize) {
    self
      .state
      .lock()
      .expect("reference cache mutex poisoned")
      .failures_remaining = count;
  }

  /// Returns the number of published action records.
  pub fn action_count(&self) -> usize {
    self.state.lock().expect("reference cache mutex poisoned").actions.len()
  }

  /// Returns the number of published blob representations.
  pub fn blob_count(&self) -> usize {
    self.state.lock().expect("reference cache mutex poisoned").blobs.len()
  }
}

fn begin(state: &ReferenceCache, headers: &HeaderMap) -> Option<Response> {
  if headers.get(header::AUTHORIZATION) != Some(&state.authorization) {
    return Some(protocol_response(StatusCode::UNAUTHORIZED, "unauthorized"));
  }
  if headers.get(REMOTE_CACHE_PROTOCOL_HEADER) != Some(&HeaderValue::from_static(REMOTE_CACHE_PROTOCOL_HEADER_VALUE_V1))
  {
    return Some(protocol_response(StatusCode::BAD_REQUEST, "unsupported protocol"));
  }
  let mut data = state.state.lock().expect("reference cache mutex poisoned");
  data.requests += 1;
  if data.failures_remaining > 0 {
    data.failures_remaining -= 1;
    let mut response = protocol_response(StatusCode::TOO_MANY_REQUESTS, "retry");
    response
      .headers_mut()
      .insert(header::RETRY_AFTER, HeaderValue::from_static("0"));
    return Some(response);
  }
  None
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

async fn get_action(
  State(state): State<ReferenceCache>,
  Path((_algorithm, hash, _size)): Path<(String, String, u64)>,
  Query(query): Query<HashMap<String, String>>,
  headers: HeaderMap,
) -> Response {
  if let Some(response) = begin(&state, &headers) {
    return response;
  }
  let namespace = query.get("namespace").cloned().unwrap_or_default();
  let data = state.state.lock().expect("reference cache mutex poisoned");
  match data.actions.get(&(namespace, hash)) {
    Some(result) => typed_response(
      StatusCode::OK,
      REMOTE_CACHE_JSON_CONTENT_TYPE,
      serde_json::to_vec(result).unwrap(),
    ),
    None => protocol_response(StatusCode::NOT_FOUND, Bytes::new()),
  }
}

async fn write_action(
  State(state): State<ReferenceCache>,
  Path((algorithm, hash, size)): Path<(String, String, u64)>,
  Query(query): Query<HashMap<String, String>>,
  headers: HeaderMap,
  Json(request): Json<WriteActionRequestV1>,
) -> Response {
  if let Some(response) = begin(&state, &headers) {
    return response;
  }
  let path_action = algorithm
    .parse::<DigestAlgorithm>()
    .and_then(|algorithm| Digest::from_hex(algorithm, &hash, size));
  if request.validate().is_err()
    || path_action.as_ref() != Ok(&request.result.action)
    || query.get("namespace") != Some(&request.namespace)
    || headers.get(header::IF_NONE_MATCH) != Some(&HeaderValue::from_static("*"))
    || headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()) != Some(REMOTE_CACHE_JSON_CONTENT_TYPE)
  {
    return protocol_response(StatusCode::BAD_REQUEST, "invalid action");
  }
  let key = (request.namespace, hash);
  let mut data = state.state.lock().expect("reference cache mutex poisoned");
  if request
    .result
    .output_bundle
    .as_ref()
    .is_some_and(|blob| !data.blobs.contains_key(&blob_key(blob)))
  {
    return protocol_response(StatusCode::CONFLICT, "missing blob");
  }
  match data.actions.get(&key) {
    Some(existing) if existing == &request.result => protocol_response(StatusCode::NO_CONTENT, Bytes::new()),
    Some(_) => protocol_response(StatusCode::CONFLICT, "conflict"),
    None => {
      data.actions.insert(key, request.result);
      protocol_response(StatusCode::CREATED, Bytes::new())
    },
  }
}

async fn find_missing(
  State(state): State<ReferenceCache>,
  headers: HeaderMap,
  Json(request): Json<FindMissingBlobsRequestV1>,
) -> Response {
  if let Some(response) = begin(&state, &headers) {
    return response;
  }
  if request.validate().is_err() {
    return protocol_response(StatusCode::BAD_REQUEST, "invalid request");
  }
  let data = state.state.lock().expect("reference cache mutex poisoned");
  let missing = request
    .blobs
    .into_iter()
    .filter(|blob| !data.blobs.contains_key(&blob_key(blob)))
    .collect();
  typed_response(
    StatusCode::OK,
    REMOTE_CACHE_JSON_CONTENT_TYPE,
    serde_json::to_vec(&FindMissingBlobsResponseV1 {
      protocol_version: REMOTE_CACHE_PROTOCOL_V1,
      missing,
    })
    .unwrap(),
  )
}

async fn read_blob(
  State(state): State<ReferenceCache>,
  Path(path): Path<(String, String, u64, String, u64, u64)>,
  headers: HeaderMap,
) -> Response {
  if let Some(response) = begin(&state, &headers) {
    return response;
  }
  let data = state.state.lock().expect("reference cache mutex poisoned");
  match data.blobs.get(&path_key(&path)) {
    Some(bytes) => typed_response(StatusCode::OK, REMOTE_CACHE_BLOB_CONTENT_TYPE, bytes.clone()),
    None => protocol_response(StatusCode::NOT_FOUND, Bytes::new()),
  }
}

async fn write_blob(
  State(state): State<ReferenceCache>,
  Path(path): Path<(String, String, u64, String, u64, u64)>,
  headers: HeaderMap,
  body: Bytes,
) -> Response {
  if let Some(response) = begin(&state, &headers) {
    return response;
  }
  let descriptor = descriptor_from_path(&path);
  if headers.get(header::IF_NONE_MATCH) != Some(&HeaderValue::from_static("*"))
    || headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok()) != Some(REMOTE_CACHE_BLOB_CONTENT_TYPE)
    || descriptor.as_ref().is_err()
    || body.len() as u64 != path.4
    || descriptor.as_ref().is_ok_and(|descriptor| {
      descriptor.encoding == BlobEncoding::Identity && descriptor.digest != Digest::blake3(&body)
    })
  {
    return protocol_response(StatusCode::BAD_REQUEST, "invalid blob");
  }
  let key = path_key(&path);
  let mut data = state.state.lock().expect("reference cache mutex poisoned");
  match data.blobs.get(&key) {
    Some(existing) if existing == &body => protocol_response(StatusCode::NO_CONTENT, Bytes::new()),
    Some(_) => protocol_response(StatusCode::CONFLICT, "conflict"),
    None => {
      data.blobs.insert(key, body.to_vec());
      protocol_response(StatusCode::CREATED, Bytes::new())
    },
  }
}

fn path_key(path: &(String, String, u64, String, u64, u64)) -> String {
  format!("{}:{}:{}:{}:{}:{}", path.0, path.1, path.2, path.3, path.4, path.5)
}

fn descriptor_from_path(path: &(String, String, u64, String, u64, u64)) -> Result<BlobDescriptor, ()> {
  let algorithm = path.0.parse::<DigestAlgorithm>().map_err(|_| ())?;
  let digest = Digest::from_hex(algorithm, &path.1, path.2).map_err(|_| ())?;
  let encoding = match path.3.as_str() {
    "identity" => BlobEncoding::Identity,
    "zstd_v1" => BlobEncoding::ZstdV1,
    _ => return Err(()),
  };
  let descriptor = BlobDescriptor {
    digest,
    encoding,
    encoded_size_bytes: path.4,
    expanded_size_bytes: path.2,
    entry_count: path.5,
  };
  descriptor.validate().map_err(|_| ())?;
  Ok(descriptor)
}

fn blob_key(blob: &BlobDescriptor) -> String {
  let encoding = match blob.encoding {
    BlobEncoding::Identity => "identity",
    BlobEncoding::ZstdV1 => "zstd_v1",
  };
  format!(
    "{}:{}:{}:{}:{}:{}",
    blob.digest.algorithm(),
    blob.digest.hex(),
    blob.expanded_size_bytes,
    encoding,
    blob.encoded_size_bytes,
    blob.entry_count
  )
}
