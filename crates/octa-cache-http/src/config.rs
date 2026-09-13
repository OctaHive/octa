//! Validated endpoint, credential, retry, and concurrency policy.

use std::{
  fs,
  io::Read as _,
  path::{Path, PathBuf},
  time::Duration,
};

use octa_cache_protocol::{
  MAX_CACHE_STRING_BYTES, MAX_CACHE_TOKEN_FILE_BYTES, MAX_REMOTE_CACHE_PARALLEL_TRANSFERS,
  MAX_REMOTE_CACHE_REQUEST_TIMEOUT_SECONDS,
};
use reqwest::Url;
use zeroize::Zeroizing;

use crate::HttpCacheError;

const MAX_CA_CERTIFICATE_FILE_BYTES: u64 = 1024 * 1024;

/// Production defaults shared by CLI profiles and runner sessions.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Default number of simultaneous remote blob transfers.
pub const DEFAULT_MAX_PARALLEL_TRANSFERS: usize = 8;
/// Default number of retries after the initial request.
pub const DEFAULT_MAX_RETRIES: u8 = 2;
/// Initial exponential retry delay.
pub const DEFAULT_RETRY_BASE_DELAY: Duration = Duration::from_millis(100);
/// Upper bound applied to exponential delay and server retry hints.
pub const DEFAULT_RETRY_MAX_DELAY: Duration = Duration::from_secs(2);
/// Consecutive failures that open the per-client circuit breaker.
pub const DEFAULT_CIRCUIT_FAILURE_THRESHOLD: u32 = 5;
/// Time for which a failed per-client circuit remains open.
pub const DEFAULT_CIRCUIT_OPEN_DURATION: Duration = Duration::from_secs(30);
/// Largest accepted whole-operation deadline.
pub const MAX_REQUEST_TIMEOUT: Duration = Duration::from_secs(MAX_REMOTE_CACHE_REQUEST_TIMEOUT_SECONDS);
/// Largest accepted transfer concurrency for one process.
pub const MAX_PARALLEL_TRANSFERS: usize = MAX_REMOTE_CACHE_PARALLEL_TRANSFERS;
/// Largest retry count accepted from operator configuration.
pub const MAX_RETRIES: u8 = 10;
/// Largest accepted retry delay, including a server `Retry-After` hint.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Largest accepted circuit-open interval.
pub const MAX_CIRCUIT_OPEN_DURATION: Duration = Duration::from_secs(60 * 60);

/// Connection-pool tuning is process-local rather than task policy, so it is
/// named here but intentionally not exposed in profiles or runner requests.
pub(crate) const CONNECTION_POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// Interval used to keep pooled TCP connections alive between cache calls.
pub(crate) const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub(crate) struct BearerToken(Zeroizing<String>);

impl BearerToken {
  fn load(path: &Path) -> Result<Self, HttpCacheError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| token_io(path, "inspect", source))?;
    validate_token_metadata(path, &metadata)?;
    let file = fs::File::open(path).map_err(|source| token_io(path, "open", source))?;
    let opened = file
      .metadata()
      .map_err(|source| token_io(path, "inspect opened", source))?;
    validate_token_metadata(path, &opened)?;
    let current = fs::symlink_metadata(path).map_err(|source| token_io(path, "re-inspect", source))?;
    let same = same_file::Handle::from_file(file.try_clone().map_err(|source| token_io(path, "retain", source))?)
      .and_then(|opened| same_file::Handle::from_path(path).map(|current| opened == current))
      .map_err(|source| token_io(path, "identify", source))?;
    if !same || !current.file_type().is_file() || current.file_type().is_symlink() {
      return Err(HttpCacheError::Configuration(format!(
        "remote cache token file '{}' changed while it was being opened",
        path.display()
      )));
    }

    // Keep the raw allocation guarded as well: invalid UTF-8 and whitespace
    // failures must not leave a credential in an ordinary dropped `Vec`.
    let mut bytes = Zeroizing::new(Vec::with_capacity(opened.len() as usize));
    file
      .take(MAX_CACHE_TOKEN_FILE_BYTES + 1)
      .read_to_end(&mut bytes)
      .map_err(|source| token_io(path, "read", source))?;
    if bytes.len() as u64 > MAX_CACHE_TOKEN_FILE_BYTES {
      return Err(token_size_error(path));
    }
    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
      bytes.pop();
    }
    let token = std::str::from_utf8(&bytes)
      .map_err(|_| HttpCacheError::Configuration("remote cache token must be UTF-8".to_owned()))?;
    if token.is_empty()
      || token
        .chars()
        .any(|character| character.is_control() || character.is_whitespace())
    {
      return Err(HttpCacheError::Configuration(
        "remote cache token must be non-empty and contain no whitespace or control characters".to_owned(),
      ));
    }
    Ok(Self(Zeroizing::new(token.to_owned())))
  }

  pub(crate) fn value(&self) -> &str {
    &self.0
  }

  #[cfg(test)]
  pub(crate) fn fixture(value: &str) -> Self {
    Self(Zeroizing::new(value.to_owned()))
  }
}

/// Bounded operational policy shared by profile and runner composition roots.
#[derive(Clone, Debug)]
pub struct HttpCachePolicy {
  /// Deadline for an entire metadata or blob operation, including transfer.
  pub request_timeout: Duration,
  /// Maximum concurrent uploads and downloads.
  pub max_parallel_transfers: usize,
  /// Retries after the initial request.
  pub max_retries: u8,
  /// Initial exponential backoff.
  pub retry_base_delay: Duration,
  /// Maximum client delay and accepted `Retry-After` hint.
  pub retry_max_delay: Duration,
  /// Consecutive failed operations before the circuit opens.
  pub circuit_failure_threshold: u32,
  /// Period during which an open circuit rejects remote work immediately.
  pub circuit_open_duration: Duration,
}

impl Default for HttpCachePolicy {
  fn default() -> Self {
    Self {
      request_timeout: DEFAULT_REQUEST_TIMEOUT,
      max_parallel_transfers: DEFAULT_MAX_PARALLEL_TRANSFERS,
      max_retries: DEFAULT_MAX_RETRIES,
      retry_base_delay: DEFAULT_RETRY_BASE_DELAY,
      retry_max_delay: DEFAULT_RETRY_MAX_DELAY,
      circuit_failure_threshold: DEFAULT_CIRCUIT_FAILURE_THRESHOLD,
      circuit_open_duration: DEFAULT_CIRCUIT_OPEN_DURATION,
    }
  }
}

impl HttpCachePolicy {
  /// Rejects values that can overflow deadlines or amplify remote failures.
  pub fn validate(&self) -> Result<(), HttpCacheError> {
    if self.request_timeout.is_zero()
      || self.request_timeout > MAX_REQUEST_TIMEOUT
      || self.max_parallel_transfers == 0
      || self.max_parallel_transfers > MAX_PARALLEL_TRANSFERS
      || self.max_retries > MAX_RETRIES
      || self.retry_max_delay < self.retry_base_delay
      || self.retry_max_delay > MAX_RETRY_DELAY
      || self.circuit_failure_threshold == 0
      || self.circuit_open_duration.is_zero()
      || self.circuit_open_duration > MAX_CIRCUIT_OPEN_DURATION
    {
      return Err(HttpCacheError::Configuration(
        "remote cache timeouts, retries, concurrency, and circuit policy exceed supported bounds".to_owned(),
      ));
    }
    Ok(())
  }
}

/// Complete configuration for one job- or CLI-run-scoped HTTP cache client.
#[derive(Clone)]
pub struct HttpCacheConfig {
  pub(crate) endpoint: Url,
  pub(crate) token: BearerToken,
  pub(crate) root_certificate: Option<reqwest::Certificate>,
  /// Retry, deadline, concurrency, and circuit-breaker policy.
  pub policy: HttpCachePolicy,
}

impl std::fmt::Debug for HttpCacheConfig {
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter
      .debug_struct("HttpCacheConfig")
      .field("endpoint", &self.endpoint)
      .field("token", &"[redacted]")
      .field("custom_root_certificate", &self.root_certificate.is_some())
      .field("policy", &self.policy)
      .finish()
  }
}

impl HttpCacheConfig {
  /// Loads a bearer credential from a stable bounded file and applies defaults.
  pub fn from_token_file(endpoint: &str, token_file: &Path) -> Result<Self, HttpCacheError> {
    Self::new(endpoint, BearerToken::load(token_file)?, false)
  }

  fn new(endpoint: &str, token: BearerToken, allow_http_loopback: bool) -> Result<Self, HttpCacheError> {
    let mut endpoint = Url::parse(endpoint)
      .map_err(|error| HttpCacheError::Configuration(format!("invalid remote cache endpoint: {error}")))?;
    let permitted_scheme = endpoint.scheme() == "https"
      || (allow_http_loopback
        && endpoint.scheme() == "http"
        && endpoint
          .host_str()
          .is_some_and(|host| host == "127.0.0.1" || host == "localhost"));
    if !permitted_scheme || endpoint.cannot_be_a_base() || endpoint.host_str().is_none() {
      return Err(HttpCacheError::Configuration(
        "remote cache endpoint must be an absolute HTTPS URL".to_owned(),
      ));
    }
    if !endpoint.username().is_empty()
      || endpoint.password().is_some()
      || endpoint.query().is_some()
      || endpoint.fragment().is_some()
    {
      return Err(HttpCacheError::Configuration(
        "remote cache endpoint must not contain credentials, a query, or a fragment".to_owned(),
      ));
    }
    if endpoint.as_str().len() > MAX_CACHE_STRING_BYTES || endpoint.as_str().chars().any(char::is_control) {
      return Err(HttpCacheError::Configuration(format!(
        "remote cache endpoint must contain at most {MAX_CACHE_STRING_BYTES} UTF-8 bytes and no control characters"
      )));
    }
    if !endpoint.path().ends_with('/') {
      endpoint.set_path(&format!("{}/", endpoint.path()));
    }
    let result = Self {
      endpoint,
      token,
      root_certificate: None,
      policy: HttpCachePolicy::default(),
    };
    result.validate()?;
    Ok(result)
  }

  /// Validates non-zero resource and timing bounds.
  pub fn validate(&self) -> Result<(), HttpCacheError> {
    self.policy.validate()
  }

  /// Adds one PEM-encoded root certificate for a private cache deployment.
  ///
  /// The certificate is public trust material, but its path is still opened
  /// with the same regular-file identity check used for operator profiles.
  pub fn with_ca_certificate_file(mut self, path: Option<&Path>) -> Result<Self, HttpCacheError> {
    let Some(path) = path else {
      return Ok(self);
    };
    let metadata = fs::symlink_metadata(path).map_err(|source| certificate_io(path, "inspect", source))?;
    if !metadata.file_type().is_file()
      || metadata.file_type().is_symlink()
      || metadata.len() == 0
      || metadata.len() > MAX_CA_CERTIFICATE_FILE_BYTES
    {
      return Err(HttpCacheError::Configuration(format!(
        "remote cache CA file '{}' must be a regular non-symlink file of 1 to {MAX_CA_CERTIFICATE_FILE_BYTES} bytes",
        path.display()
      )));
    }
    let file = fs::File::open(path).map_err(|source| certificate_io(path, "open", source))?;
    let opened = file
      .metadata()
      .map_err(|source| certificate_io(path, "inspect opened", source))?;
    let opened_handle = same_file::Handle::from_file(
      file
        .try_clone()
        .map_err(|source| certificate_io(path, "retain", source))?,
    )
    .map_err(|source| certificate_io(path, "identify opened", source))?;
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file
      .take(MAX_CA_CERTIFICATE_FILE_BYTES + 1)
      .read_to_end(&mut bytes)
      .map_err(|source| certificate_io(path, "read", source))?;
    let current = fs::symlink_metadata(path).map_err(|source| certificate_io(path, "re-inspect", source))?;
    let same = same_file::Handle::from_path(path)
      .map(|current| opened_handle == current)
      .map_err(|source| certificate_io(path, "identify", source))?;
    if !same
      || !opened.is_file()
      || opened.len() != metadata.len()
      || current.file_type().is_symlink()
      || current.len() != metadata.len()
      || bytes.len() as u64 > MAX_CA_CERTIFICATE_FILE_BYTES
    {
      return Err(HttpCacheError::Configuration(format!(
        "remote cache CA file '{}' changed while it was being read",
        path.display()
      )));
    }
    let pem = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
    let pem = pem.strip_suffix(b"\r").unwrap_or(pem);
    if !pem.starts_with(b"-----BEGIN CERTIFICATE-----") || !pem.ends_with(b"-----END CERTIFICATE-----") {
      return Err(HttpCacheError::Configuration(format!(
        "remote cache CA file '{}' must contain PEM certificate data",
        path.display()
      )));
    }
    self.root_certificate = Some(reqwest::Certificate::from_pem(&bytes)?);
    Ok(self)
  }

  #[cfg(test)]
  pub(crate) fn loopback(endpoint: &str) -> Self {
    Self::new(endpoint, BearerToken::fixture("fixture-token"), true).unwrap()
  }

  #[cfg(test)]
  pub(crate) fn new_for_test(endpoint: &str) -> Result<Self, HttpCacheError> {
    Self::new(endpoint, BearerToken::fixture("fixture-token"), false)
  }
}

fn validate_token_metadata(path: &Path, metadata: &fs::Metadata) -> Result<(), HttpCacheError> {
  if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
    return Err(HttpCacheError::Configuration(format!(
      "remote cache token file '{}' must be a regular non-symlink file",
      path.display()
    )));
  }
  if metadata.len() == 0 || metadata.len() > MAX_CACHE_TOKEN_FILE_BYTES {
    return Err(token_size_error(path));
  }
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt as _;
    // SAFETY: `geteuid` has no pointer arguments or preconditions and merely
    // reads the effective uid of the current process.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
      return Err(HttpCacheError::Configuration(format!(
        "remote cache token file '{}' must be owned by the current process user",
        path.display()
      )));
    }
    if metadata.mode() & 0o077 != 0 {
      return Err(HttpCacheError::Configuration(format!(
        "remote cache token file '{}' must not grant group or other permissions",
        path.display()
      )));
    }
  }
  Ok(())
}

fn token_io(path: &Path, operation: &'static str, source: std::io::Error) -> HttpCacheError {
  HttpCacheError::TokenFile {
    operation,
    path: PathBuf::from(path),
    source,
  }
}

fn certificate_io(path: &Path, operation: &'static str, source: std::io::Error) -> HttpCacheError {
  HttpCacheError::CertificateFile {
    operation,
    path: path.to_path_buf(),
    source,
  }
}

fn token_size_error(path: &Path) -> HttpCacheError {
  HttpCacheError::Configuration(format!(
    "remote cache token file '{}' must contain between 1 and {MAX_CACHE_TOKEN_FILE_BYTES} bytes",
    path.display()
  ))
}
