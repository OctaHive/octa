//! Job-scoped secret resolution with bounded I/O and redacted values.
//!
//! Profiles contain provider configuration only. Resolved values remain in a
//! [`SecretSession`] cache and are never exposed through `Debug` or `Display`.

use std::{
  collections::HashMap,
  fmt,
  path::{Component, Path, PathBuf},
  sync::Arc,
  time::{Duration, Instant},
};

use octa_octafile::SecretRef;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
  io::{AsyncReadExt, AsyncWriteExt},
  process::Command,
  sync::Mutex,
  time::timeout,
};
use tokio_util::sync::CancellationToken;

use crate::error::{ExecutorError, ExecutorResult};

const SECRET_PROFILE_VERSION: u8 = 1;
const MAX_SECRET_BYTES: usize = 1024 * 1024;
const DEFAULT_CACHE_TTL_SECONDS: u64 = 300;
const DEFAULT_EXEC_TIMEOUT_SECONDS: u64 = 30;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretProfile {
  pub version: u8,
  #[serde(default = "default_cache_ttl")]
  pub cache_ttl_seconds: u64,
  pub providers: HashMap<String, SecretProviderConfig>,
}

impl fmt::Debug for SecretProfile {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut providers = self.providers.keys().collect::<Vec<_>>();
    providers.sort_unstable();
    formatter
      .debug_struct("SecretProfile")
      .field("version", &self.version)
      .field("providers", &providers)
      .finish()
  }
}

impl SecretProfile {
  /// Loads, validates, and resolves relative paths in a secret profile.
  pub fn load(path: &Path) -> ExecutorResult<Self> {
    let contents = std::fs::read_to_string(path).map_err(|error| ExecutorError::SecretProfile {
      message: format!("failed to read '{}': {error}", path.display()),
    })?;
    let mut profile: Self = serde_yml::from_str(&contents).map_err(|error| ExecutorError::SecretProfile {
      message: format!("failed to parse '{}': {error}", path.display()),
    })?;
    if profile.version != SECRET_PROFILE_VERSION {
      return Err(ExecutorError::SecretProfile {
        message: format!("unsupported version {}", profile.version),
      });
    }
    if profile.cache_ttl_seconds == 0 {
      return Err(ExecutorError::SecretProfile {
        message: "cache_ttl_seconds must be greater than zero".to_owned(),
      });
    }
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    for (alias, provider) in &mut profile.providers {
      if alias.trim().is_empty() {
        return Err(ExecutorError::SecretProfile {
          message: "provider alias must not be empty".to_owned(),
        });
      }
      provider.validate(alias)?;
      provider.resolve_paths(base);
    }
    Ok(profile)
  }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum SecretProviderConfig {
  Env {
    #[serde(default)]
    prefix: String,
  },
  File {
    root: PathBuf,
  },
  Exec {
    command: Vec<String>,
    #[serde(default = "default_exec_timeout")]
    timeout_seconds: u64,
  },
  Vault {
    address: String,
    #[serde(default = "default_vault_mount")]
    mount: String,
    #[serde(default = "default_vault_kv_version")]
    kv_version: u8,
    #[serde(default)]
    namespace: Option<String>,
    auth: VaultAuth,
  },
}

impl SecretProviderConfig {
  fn resolve_paths(&mut self, base: &Path) {
    match self {
      Self::File { root } if !root.is_absolute() => *root = base.join(&*root),
      Self::Vault {
        auth: VaultAuth::Jwt { jwt_path, .. },
        ..
      } if !jwt_path.is_absolute() => *jwt_path = base.join(&*jwt_path),
      _ => {},
    }
  }

  fn validate(&self, alias: &str) -> ExecutorResult<()> {
    let invalid = |message: &str| ExecutorError::SecretProfile {
      message: format!("provider '{alias}': {message}"),
    };
    match self {
      Self::Env { .. } => {},
      Self::File { root } if root.as_os_str().is_empty() => return Err(invalid("root must not be empty")),
      Self::File { .. } => {},
      Self::Exec {
        command,
        timeout_seconds,
      } => {
        if command.is_empty() || command.iter().any(|part| part.is_empty()) {
          return Err(invalid("command must contain a program and no empty arguments"));
        }
        if *timeout_seconds == 0 {
          return Err(invalid("timeout_seconds must be greater than zero"));
        }
      },
      Self::Vault {
        address,
        mount,
        kv_version,
        auth,
        ..
      } => {
        let address = Url::parse(address).map_err(|_| invalid("address must be a valid URL"))?;
        if !matches!(address.scheme(), "http" | "https")
          || address.cannot_be_a_base()
          || address.query().is_some()
          || address.fragment().is_some()
        {
          return Err(invalid("address must be an http(s) base URL without query or fragment"));
        }
        validate_vault_path(mount).map_err(|message| invalid(&format!("mount {message}")))?;
        if !matches!(kv_version, 1 | 2) {
          return Err(invalid("kv_version must be 1 or 2"));
        }
        auth.validate(alias)?;
      },
    }
    Ok(())
  }
}

#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VaultAuth {
  Token {
    #[serde(default = "default_vault_token_env")]
    token_env: String,
  },
  Jwt {
    role: String,
    jwt_path: PathBuf,
    #[serde(default = "default_vault_jwt_mount")]
    mount: String,
  },
}

impl VaultAuth {
  fn validate(&self, alias: &str) -> ExecutorResult<()> {
    let message = match self {
      Self::Token { token_env } if token_env.trim().is_empty() => Some("token_env must not be empty"),
      Self::Jwt { role, .. } if role.trim().is_empty() => Some("JWT role must not be empty"),
      Self::Jwt { jwt_path, .. } if jwt_path.as_os_str().is_empty() => Some("JWT path must not be empty"),
      Self::Jwt { mount, .. } if validate_vault_path(mount).is_err() => Some("JWT mount must be a safe path"),
      _ => None,
    };
    if let Some(message) = message {
      return Err(ExecutorError::SecretProfile {
        message: format!("provider '{alias}': {message}"),
      });
    }
    Ok(())
  }
}

#[derive(Clone)]
pub struct SecretSession {
  profile: Arc<SecretProfile>,
  client: Client,
  cache: Arc<Mutex<HashMap<SecretRef, CachedSecret>>>,
  vault_tokens: Arc<Mutex<HashMap<String, VaultToken>>>,
}

impl fmt::Debug for SecretSession {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter
      .debug_struct("SecretSession")
      .field("profile", &self.profile)
      .finish_non_exhaustive()
  }
}

impl SecretSession {
  /// Creates one isolated cache and Vault-token lifecycle for a job.
  pub fn new(profile: SecretProfile) -> ExecutorResult<Self> {
    let client = Client::builder()
      .timeout(Duration::from_secs(30))
      .build()
      .map_err(|error| ExecutorError::SecretProvider {
        provider: "http".to_owned(),
        message: error.to_string(),
      })?;
    Ok(Self {
      profile: Arc::new(profile),
      client,
      cache: Arc::new(Mutex::new(HashMap::new())),
      vault_tokens: Arc::new(Mutex::new(HashMap::new())),
    })
  }

  /// Resolves a reference through its configured provider with cooperative cancellation.
  pub async fn resolve(&self, reference: &SecretRef, cancellation: &CancellationToken) -> ExecutorResult<SecretValue> {
    if let Some(cached) = self
      .cache
      .lock()
      .await
      .get(reference)
      .filter(|cached| cached.expires > Instant::now())
    {
      return Ok(cached.value.clone());
    }
    let provider = self
      .profile
      .providers
      .get(&reference.provider)
      .ok_or_else(|| ExecutorError::SecretProvider {
        provider: reference.provider.clone(),
        message: "unknown provider alias".to_owned(),
      })?;
    let value = tokio::select! {
      biased;
      _ = cancellation.cancelled() => return Err(ExecutorError::TaskCancelled("secret resolution".to_owned())),
      result = self.resolve_provider(&reference.provider, provider, reference) => result?,
    };
    let value = SecretValue(value);
    self.cache.lock().await.insert(
      reference.clone(),
      CachedSecret {
        value: value.clone(),
        expires: Instant::now() + Duration::from_secs(self.profile.cache_ttl_seconds),
      },
    );
    Ok(value)
  }

  async fn resolve_provider(
    &self,
    alias: &str,
    provider: &SecretProviderConfig,
    reference: &SecretRef,
  ) -> ExecutorResult<Value> {
    match provider {
      SecretProviderConfig::Env { prefix } => {
        let value = std::env::var(format!("{prefix}{}", reference.key)).map_err(|_| ExecutorError::SecretProvider {
          provider: alias.to_owned(),
          message: format!("environment key '{}' is not set", reference.key),
        })?;
        select_field(alias, &reference.key, value, reference.field.as_deref())
      },
      SecretProviderConfig::File { root } => {
        let path = safe_secret_path(alias, root, &reference.key)?;
        let bytes = read_bounded(&path)
          .await
          .map_err(|message| ExecutorError::SecretProvider {
            provider: alias.to_owned(),
            message,
          })?;
        let value = String::from_utf8(bytes).map_err(|_| ExecutorError::SecretProvider {
          provider: alias.to_owned(),
          message: format!("key '{}' is not UTF-8", reference.key),
        })?;
        select_field(
          alias,
          &reference.key,
          value.trim_end().to_owned(),
          reference.field.as_deref(),
        )
      },
      SecretProviderConfig::Exec {
        command,
        timeout_seconds,
      } => self.resolve_exec(alias, command, *timeout_seconds, reference).await,
      SecretProviderConfig::Vault { .. } => self.resolve_vault(alias, provider, reference).await,
    }
  }

  async fn resolve_exec(
    &self,
    alias: &str,
    command: &[String],
    timeout_seconds: u64,
    reference: &SecretRef,
  ) -> ExecutorResult<Value> {
    let mut child = Command::new(&command[0]);
    child
      .args(&command[1..])
      .stdin(std::process::Stdio::piped())
      .stdout(std::process::Stdio::piped())
      .stderr(std::process::Stdio::null())
      .kill_on_drop(true);
    let mut child = child
      .spawn()
      .map_err(|error| provider_error(alias, format!("failed to start: {error}")))?;
    let mut stdin = child
      .stdin
      .take()
      .ok_or_else(|| provider_error(alias, "stdin is unavailable"))?;
    let mut stdout = child
      .stdout
      .take()
      .ok_or_else(|| provider_error(alias, "stdout is unavailable"))?;
    let request = serde_json::to_vec(reference).map_err(|error| provider_error(alias, error.to_string()))?;
    let operation = async {
      let send_result = stdin.write_all(&request).await;
      drop(stdin);
      let mut bytes = Vec::new();
      (&mut stdout)
        .take((MAX_SECRET_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .await?;
      let status = child.wait().await?;
      Ok::<_, std::io::Error>((status, bytes, send_result))
    };
    let (status, bytes, send_result) = timeout(Duration::from_secs(timeout_seconds), operation)
      .await
      .map_err(|_| provider_error(alias, "command timed out"))?
      .map_err(|error| provider_error(alias, format!("command failed: {error}")))?;
    if !status.success() {
      return Err(provider_error(alias, "command returned a non-zero status"));
    }
    send_result.map_err(|error| provider_error(alias, format!("failed to send request: {error}")))?;
    if bytes.len() > MAX_SECRET_BYTES {
      return Err(provider_error(alias, "value exceeds the 1 MiB limit"));
    }
    let value = String::from_utf8(bytes).map_err(|_| provider_error(alias, "command output is not UTF-8"))?;
    select_field(
      alias,
      &reference.key,
      value.trim_end().to_owned(),
      reference.field.as_deref(),
    )
  }

  async fn resolve_vault(
    &self,
    alias: &str,
    provider: &SecretProviderConfig,
    reference: &SecretRef,
  ) -> ExecutorResult<Value> {
    let SecretProviderConfig::Vault {
      address,
      mount,
      kv_version,
      namespace,
      auth,
    } = provider
    else {
      unreachable!("resolve_vault is called only for a Vault provider")
    };
    let namespace = namespace.as_deref();
    let token = self.vault_token(alias, address, namespace, auth).await?;
    validate_vault_path(&reference.key).map_err(|message| provider_error(alias, format!("key {message}")))?;
    let mut url = Url::parse(address).map_err(|_| provider_error(alias, "Vault address is invalid"))?;
    {
      let mut segments = url
        .path_segments_mut()
        .map_err(|_| provider_error(alias, "Vault address cannot contain path segments"))?;
      segments.pop_if_empty().push("v1");
      for segment in mount.trim_matches('/').split('/') {
        segments.push(segment);
      }
      if *kv_version == 2 {
        segments.push("data");
      }
      for segment in reference.key.trim_matches('/').split('/') {
        segments.push(segment);
      }
    }
    let mut request = self.client.get(url).header("X-Vault-Token", token);
    if let Some(namespace) = namespace {
      request = request.header("X-Vault-Namespace", namespace);
    }
    let response = request
      .send()
      .await
      .map_err(|error| provider_error(alias, format!("request failed: {error}")))?;
    if response.status() != StatusCode::OK {
      return Err(provider_error(
        alias,
        format!("Vault returned status {}", response.status()),
      ));
    }
    let body = response_json(alias, response).await?;
    let data = if *kv_version == 2 {
      &body["data"]["data"]
    } else {
      &body["data"]
    };
    if data.is_null() {
      return Err(provider_error(alias, format!("key '{}' has no data", reference.key)));
    }
    match &reference.field {
      Some(field) => data
        .get(field)
        .cloned()
        .ok_or_else(|| provider_error(alias, format!("key '{}' has no field '{field}'", reference.key))),
      None => Ok(data.clone()),
    }
  }

  async fn vault_token(
    &self,
    alias: &str,
    address: &str,
    namespace: Option<&str>,
    auth: &VaultAuth,
  ) -> ExecutorResult<String> {
    let cached_token = { self.vault_tokens.lock().await.get(alias).cloned() };
    if let Some(token) = cached_token {
      if token
        .expires
        .is_none_or(|expires| expires > Instant::now() + Duration::from_secs(30))
      {
        return Ok(token.value);
      }
      if token.owned && token.renewable {
        if let Ok(renewed) = self.renew_vault_token(alias, token).await {
          let value = renewed.value.clone();
          self.vault_tokens.lock().await.insert(alias.to_owned(), renewed);
          return Ok(value);
        }
        // A failed renewal is followed by a fresh JWT login below. Error responses are
        // intentionally discarded because they may contain provider-specific details.
      }
    }
    match auth {
      VaultAuth::Token { token_env } => {
        let value =
          std::env::var(token_env).map_err(|_| provider_error(alias, format!("token env '{token_env}' is not set")))?;
        self.vault_tokens.lock().await.insert(
          alias.to_owned(),
          VaultToken {
            value: value.clone(),
            expires: None,
            renewable: false,
            owned: false,
            address: address.to_owned(),
            namespace: namespace.map(str::to_owned),
          },
        );
        Ok(value)
      },
      VaultAuth::Jwt { role, jwt_path, mount } => {
        let jwt = String::from_utf8(
          read_bounded(jwt_path)
            .await
            .map_err(|message| provider_error(alias, message))?,
        )
        .map_err(|_| provider_error(alias, "JWT is not UTF-8"))?;
        let mut url = Url::parse(address).map_err(|_| provider_error(alias, "Vault address is invalid"))?;
        {
          let mut segments = url
            .path_segments_mut()
            .map_err(|_| provider_error(alias, "Vault address cannot contain path segments"))?;
          segments.pop_if_empty().push("v1");
          for segment in mount.trim_matches('/').split('/') {
            segments.push(segment);
          }
          segments.push("login");
        }
        let mut request = self
          .client
          .post(url)
          .json(&serde_json::json!({ "role": role, "jwt": jwt.trim() }));
        if let Some(namespace) = namespace {
          request = request.header("X-Vault-Namespace", namespace);
        }
        let response = request
          .send()
          .await
          .map_err(|error| provider_error(alias, format!("login failed: {error}")))?;
        if response.status() != StatusCode::OK {
          return Err(provider_error(
            alias,
            format!("Vault login returned status {}", response.status()),
          ));
        }
        let body = response_json(alias, response).await?;
        let value = body["auth"]["client_token"]
          .as_str()
          .ok_or_else(|| provider_error(alias, "Vault login omitted client_token"))?
          .to_owned();
        let lease = body["auth"]["lease_duration"].as_u64().unwrap_or(300);
        let token = VaultToken {
          value: value.clone(),
          expires: Some(Instant::now() + Duration::from_secs(lease.max(1))),
          renewable: body["auth"]["renewable"].as_bool().unwrap_or(false),
          owned: true,
          address: address.to_owned(),
          namespace: namespace.map(str::to_owned),
        };
        self.vault_tokens.lock().await.insert(alias.to_owned(), token);
        Ok(value)
      },
    }
  }

  async fn renew_vault_token(&self, alias: &str, token: VaultToken) -> ExecutorResult<VaultToken> {
    let mut request = self
      .client
      .post(format!(
        "{}/v1/auth/token/renew-self",
        token.address.trim_end_matches('/')
      ))
      .header("X-Vault-Token", &token.value);
    if let Some(namespace) = &token.namespace {
      request = request.header("X-Vault-Namespace", namespace);
    }
    let response = request
      .send()
      .await
      .map_err(|error| provider_error(alias, format!("token renewal failed: {error}")))?;
    if response.status() != StatusCode::OK {
      return Err(provider_error(
        alias,
        format!("Vault token renewal returned status {}", response.status()),
      ));
    }
    let body = response_json(alias, response).await?;
    let lease = body["auth"]["lease_duration"].as_u64().unwrap_or(300).max(1);
    Ok(VaultToken {
      value: body["auth"]["client_token"].as_str().unwrap_or(&token.value).to_owned(),
      expires: Some(Instant::now() + Duration::from_secs(lease)),
      renewable: body["auth"]["renewable"].as_bool().unwrap_or(token.renewable),
      ..token
    })
  }

  /// Revokes only tokens obtained through this session's JWT login.
  pub async fn shutdown(&self) {
    let tokens = self
      .vault_tokens
      .lock()
      .await
      .drain()
      .map(|(_, token)| token)
      .collect::<Vec<_>>();
    for token in tokens.into_iter().filter(|token| token.owned) {
      let mut request = self
        .client
        .post(format!(
          "{}/v1/auth/token/revoke-self",
          token.address.trim_end_matches('/')
        ))
        .header("X-Vault-Token", token.value);
      if let Some(namespace) = token.namespace {
        request = request.header("X-Vault-Namespace", namespace);
      }
      let _ = request.send().await;
    }
    self.cache.lock().await.clear();
  }
}

#[derive(Clone)]
/// A redacted wrapper whose contents can only enter the executor's value pipeline.
pub struct SecretValue(Value);

impl SecretValue {
  pub(crate) fn into_value(self) -> Value {
    self.0
  }
}

impl fmt::Debug for SecretValue {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("SecretValue(*****)")
  }
}

impl fmt::Display for SecretValue {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.write_str("*****")
  }
}

#[derive(Clone)]
struct CachedSecret {
  value: SecretValue,
  expires: Instant,
}

#[derive(Clone)]
struct VaultToken {
  value: String,
  expires: Option<Instant>,
  renewable: bool,
  owned: bool,
  address: String,
  namespace: Option<String>,
}

fn safe_secret_path(alias: &str, root: &Path, key: &str) -> ExecutorResult<PathBuf> {
  let key = Path::new(key);
  if key.is_absolute()
    || key
      .components()
      .any(|component| !matches!(component, Component::Normal(_)))
  {
    return Err(provider_error(alias, "file key must be a safe relative path"));
  }
  let root =
    dunce::canonicalize(root).map_err(|error| provider_error(alias, format!("cannot resolve root: {error}")))?;
  let path = dunce::canonicalize(root.join(key))
    .map_err(|error| provider_error(alias, format!("cannot resolve key: {error}")))?;
  if !path.starts_with(root) {
    return Err(provider_error(alias, "file key escapes the provider root"));
  }
  Ok(path)
}

async fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
  let mut file = tokio::fs::File::open(path)
    .await
    .map_err(|error| format!("failed to open '{}': {error}", path.display()))?;
  let mut bytes = Vec::new();
  (&mut file)
    .take((MAX_SECRET_BYTES + 1) as u64)
    .read_to_end(&mut bytes)
    .await
    .map_err(|error| format!("failed to read '{}': {error}", path.display()))?;
  if bytes.len() > MAX_SECRET_BYTES {
    return Err(format!("'{}' exceeds the 1 MiB limit", path.display()));
  }
  Ok(bytes)
}

fn select_field(alias: &str, key: &str, value: String, field: Option<&str>) -> ExecutorResult<Value> {
  let Some(field) = field else {
    return Ok(Value::String(value));
  };
  let document: Value = serde_json::from_str(&value)
    .map_err(|_| provider_error(alias, format!("key '{key}' is not JSON but requests field '{field}'")))?;
  document
    .get(field)
    .cloned()
    .ok_or_else(|| provider_error(alias, format!("key '{key}' has no field '{field}'")))
}

async fn response_json(alias: &str, mut response: reqwest::Response) -> ExecutorResult<Value> {
  let mut bytes = Vec::new();
  while let Some(chunk) = response
    .chunk()
    .await
    .map_err(|error| provider_error(alias, format!("failed to read response: {error}")))?
  {
    if bytes.len().saturating_add(chunk.len()) > MAX_SECRET_BYTES {
      return Err(provider_error(alias, "response exceeds the 1 MiB limit"));
    }
    bytes.extend_from_slice(&chunk);
  }
  serde_json::from_slice(&bytes).map_err(|_| provider_error(alias, "response is not valid JSON"))
}

fn provider_error(provider: &str, message: impl Into<String>) -> ExecutorError {
  ExecutorError::SecretProvider {
    provider: provider.to_owned(),
    message: message.into(),
  }
}

fn validate_vault_path(path: &str) -> Result<(), &'static str> {
  let path = path.trim_matches('/');
  if path.is_empty()
    || path
      .split('/')
      .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
  {
    return Err("must be a non-empty path without empty, '.' or '..' segments");
  }
  Ok(())
}

fn default_cache_ttl() -> u64 {
  DEFAULT_CACHE_TTL_SECONDS
}

fn default_exec_timeout() -> u64 {
  DEFAULT_EXEC_TIMEOUT_SECONDS
}

fn default_vault_mount() -> String {
  "secret".to_owned()
}

fn default_vault_kv_version() -> u8 {
  2
}

fn default_vault_token_env() -> String {
  "VAULT_TOKEN".to_owned()
}

fn default_vault_jwt_mount() -> String {
  "auth/jwt".to_owned()
}

#[cfg(test)]
mod tests {
  use std::fs;

  use tempfile::TempDir;
  use tokio::net::{TcpListener, TcpStream};

  use super::*;

  #[test]
  fn secret_values_never_format_their_contents() {
    let value = SecretValue(Value::String("do-not-print".to_owned()));
    assert_eq!(format!("{value}"), "*****");
    assert_eq!(format!("{value:?}"), "SecretValue(*****)");
  }

  #[test]
  fn vault_paths_reject_traversal_and_empty_segments() {
    assert!(validate_vault_path("service/token").is_ok());
    assert!(validate_vault_path("../auth/token").is_err());
    assert!(validate_vault_path("service//token").is_err());
  }

  #[test]
  fn profile_loads_defaults_and_resolves_relative_paths() {
    let directory = TempDir::new().unwrap();
    fs::create_dir(directory.path().join("files")).unwrap();
    fs::write(directory.path().join("jwt"), "signed").unwrap();
    let path = directory.path().join("secrets.yml");
    fs::write(
      &path,
      r#"version: 1
providers:
  files:
    type: file
    root: files
  helper:
    type: exec
    command: [echo]
  vault:
    type: vault
    address: http://127.0.0.1:8200
    auth:
      type: jwt
      role: ci
      jwt_path: jwt
"#,
    )
    .unwrap();

    let profile = SecretProfile::load(&path).unwrap();
    assert_eq!(profile.cache_ttl_seconds, DEFAULT_CACHE_TTL_SECONDS);
    assert!(matches!(
      &profile.providers["files"],
      SecretProviderConfig::File { root } if root == &directory.path().join("files")
    ));
    assert!(matches!(
      &profile.providers["helper"],
      SecretProviderConfig::Exec {
        timeout_seconds: DEFAULT_EXEC_TIMEOUT_SECONDS,
        ..
      }
    ));
    assert!(matches!(
      &profile.providers["vault"],
      SecretProviderConfig::Vault { mount, kv_version: 2, auth: VaultAuth::Jwt { jwt_path, mount: auth_mount, .. }, .. }
        if mount == "secret" && auth_mount == "auth/jwt" && jwt_path == &directory.path().join("jwt")
    ));
    assert_eq!(
      format!("{profile:?}"),
      "SecretProfile { version: 1, providers: [\"files\", \"helper\", \"vault\"] }"
    );
  }

  #[test]
  fn profile_rejects_invalid_versions_and_provider_settings() {
    let directory = TempDir::new().unwrap();
    let path = directory.path().join("secrets.yml");
    assert!(matches!(
      SecretProfile::load(&path),
      Err(ExecutorError::SecretProfile { .. })
    ));
    fs::write(&path, "not: [valid").unwrap();
    assert!(matches!(
      SecretProfile::load(&path),
      Err(ExecutorError::SecretProfile { .. })
    ));

    let cases = [
      "version: 2\nproviders: {}\n",
      "version: 1\ncache_ttl_seconds: 0\nproviders: {}\n",
      "version: 1\nproviders:\n  ' ': {type: env}\n",
      "version: 1\nproviders:\n  bad: {type: file, root: ''}\n",
      "version: 1\nproviders:\n  bad: {type: exec, command: []}\n",
      "version: 1\nproviders:\n  bad: {type: exec, command: [echo], timeout_seconds: 0}\n",
      "version: 1\nproviders:\n  bad: {type: vault, address: ftp://vault, auth: {type: token}}\n",
      "version: 1\nproviders:\n  bad: {type: vault, address: 'http://vault?x=1', auth: {type: token}}\n",
      "version: 1\nproviders:\n  bad: {type: vault, address: http://vault, mount: '../secret', auth: {type: token}}\n",
      "version: 1\nproviders:\n  bad: {type: vault, address: http://vault, kv_version: 3, auth: {type: token}}\n",
      "version: 1\nproviders:\n  bad: {type: vault, address: http://vault, auth: {type: token, token_env: ''}}\n",
      "version: 1\nproviders:\n  bad: {type: vault, address: http://vault, auth: {type: jwt, role: '', jwt_path: jwt}}\n",
      "version: 1\nproviders:\n  bad: {type: vault, address: http://vault, auth: {type: jwt, role: ci, jwt_path: jwt, mount: '../jwt'}}\n",
    ];
    for contents in cases {
      fs::write(&path, contents).unwrap();
      assert!(
        matches!(SecretProfile::load(&path), Err(ExecutorError::SecretProfile { .. })),
        "profile unexpectedly accepted: {contents}"
      );
    }
  }

  #[tokio::test]
  async fn env_provider_resolves_prefixed_keys() {
    let key = format!("OCTA_SECRET_TEST_{}_TOKEN", std::process::id());
    std::env::set_var(&key, "environment-value");
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::from([(
        "environment".to_owned(),
        SecretProviderConfig::Env {
          prefix: format!("OCTA_SECRET_TEST_{}_", std::process::id()),
        },
      )]),
    })
    .unwrap();
    let value = session
      .resolve(
        &SecretRef {
          provider: "environment".to_owned(),
          key: "TOKEN".to_owned(),
          field: None,
        },
        &CancellationToken::new(),
      )
      .await
      .unwrap();
    std::env::set_var(&key, "changed-after-cache");
    let cached = session
      .resolve(
        &SecretRef {
          provider: "environment".to_owned(),
          key: "TOKEN".to_owned(),
          field: None,
        },
        &CancellationToken::new(),
      )
      .await
      .unwrap();
    std::env::remove_var(key);
    assert_eq!(value.into_value(), serde_json::json!("environment-value"));
    assert_eq!(cached.into_value(), serde_json::json!("environment-value"));
  }

  #[tokio::test]
  async fn resolution_reports_unknown_missing_and_cancelled_providers() {
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::from([(
        "environment".to_owned(),
        SecretProviderConfig::Env { prefix: String::new() },
      )]),
    })
    .unwrap();
    let unknown = SecretRef {
      provider: "missing".to_owned(),
      key: "key".to_owned(),
      field: None,
    };
    assert!(session.resolve(&unknown, &CancellationToken::new()).await.is_err());

    let missing = SecretRef {
      provider: "environment".to_owned(),
      key: format!("OCTA_MISSING_SECRET_{}", std::process::id()),
      field: None,
    };
    assert!(session.resolve(&missing, &CancellationToken::new()).await.is_err());
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(matches!(
      session.resolve(&missing, &cancellation).await,
      Err(ExecutorError::TaskCancelled(_))
    ));
  }

  #[tokio::test]
  async fn file_provider_resolves_fields_and_rejects_escape() {
    let directory = TempDir::new().unwrap();
    let root = directory.path().join("secrets");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("service.json"), r#"{"token":"resolved"}"#).unwrap();
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::from([("app".to_owned(), SecretProviderConfig::File { root })]),
    })
    .unwrap();

    let value = session
      .resolve(
        &SecretRef {
          provider: "app".to_owned(),
          key: "service.json".to_owned(),
          field: Some("token".to_owned()),
        },
        &CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(value.into_value(), serde_json::json!("resolved"));

    let error = session
      .resolve(
        &SecretRef {
          provider: "app".to_owned(),
          key: "../outside".to_owned(),
          field: None,
        },
        &CancellationToken::new(),
      )
      .await
      .unwrap_err();
    assert!(error.to_string().contains("safe relative path"));

    #[cfg(unix)]
    {
      use std::os::unix::fs::symlink;

      let outside = tempfile::NamedTempFile::new().unwrap();
      symlink(outside.path(), directory.path().join("secrets/linked")).unwrap();
      let error = session
        .resolve(
          &SecretRef {
            provider: "app".to_owned(),
            key: "linked".to_owned(),
            field: None,
          },
          &CancellationToken::new(),
        )
        .await
        .unwrap_err();
      assert!(error.to_string().contains("escapes the provider root"));
    }
  }

  #[tokio::test]
  async fn file_provider_bounds_and_validates_file_contents() {
    let directory = TempDir::new().unwrap();
    fs::write(directory.path().join("invalid.json"), "not-json").unwrap();
    fs::write(directory.path().join("fields.json"), r#"{"present":true}"#).unwrap();
    fs::write(directory.path().join("binary"), [0xff]).unwrap();
    fs::write(directory.path().join("large"), vec![b'x'; MAX_SECRET_BYTES + 1]).unwrap();
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::from([(
        "files".to_owned(),
        SecretProviderConfig::File {
          root: directory.path().to_path_buf(),
        },
      )]),
    })
    .unwrap();
    for (key, field, expected) in [
      ("invalid.json", Some("value"), "is not JSON"),
      ("fields.json", Some("missing"), "has no field"),
      ("binary", None, "not UTF-8"),
      ("large", None, "exceeds the 1 MiB limit"),
      ("absent", None, "cannot resolve key"),
    ] {
      let error = session
        .resolve(
          &SecretRef {
            provider: "files".to_owned(),
            key: key.to_owned(),
            field: field.map(str::to_owned),
          },
          &CancellationToken::new(),
        )
        .await
        .unwrap_err();
      assert!(error.to_string().contains(expected), "unexpected error: {error}");
    }
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn exec_provider_receives_a_typed_request() {
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::from([(
        "helper".to_owned(),
        SecretProviderConfig::Exec {
          command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "request=$(cat); case \"$request\" in *'\"key\":\"token\"'*) printf resolved;; *) exit 9;; esac".to_owned(),
          ],
          timeout_seconds: 5,
        },
      )]),
    })
    .unwrap();
    let value = session
      .resolve(
        &SecretRef {
          provider: "helper".to_owned(),
          key: "token".to_owned(),
          field: None,
        },
        &CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(value.into_value(), serde_json::json!("resolved"));
  }

  #[cfg(unix)]
  #[tokio::test]
  async fn exec_provider_reports_process_failures_and_enforces_limits() {
    for (script, timeout_seconds, expected) in [
      ("exit 7", 2, "non-zero"),
      ("printf '\\377'", 2, "not UTF-8"),
      ("head -c 1048577 /dev/zero", 2, "1 MiB limit"),
      ("sleep 2", 1, "timed out"),
    ] {
      let session = SecretSession::new(SecretProfile {
        version: 1,
        cache_ttl_seconds: 60,
        providers: HashMap::from([(
          "helper".to_owned(),
          SecretProviderConfig::Exec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
            timeout_seconds,
          },
        )]),
      })
      .unwrap();
      let error = session
        .resolve(
          &SecretRef {
            provider: "helper".to_owned(),
            key: "token".to_owned(),
            field: None,
          },
          &CancellationToken::new(),
        )
        .await
        .unwrap_err();
      assert!(error.to_string().contains(expected), "unexpected error: {error}");
    }

    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::from([(
        "helper".to_owned(),
        SecretProviderConfig::Exec {
          command: vec!["/definitely/missing/octa-secret-helper".to_owned()],
          timeout_seconds: 1,
        },
      )]),
    })
    .unwrap();
    let error = session
      .resolve(
        &SecretRef {
          provider: "helper".to_owned(),
          key: "token".to_owned(),
          field: None,
        },
        &CancellationToken::new(),
      )
      .await
      .unwrap_err();
    assert!(error.to_string().contains("failed to start"));
  }

  #[tokio::test]
  async fn vault_token_auth_resolves_kv_one_and_two() {
    let token_env = format!("OCTA_VAULT_TOKEN_{}", std::process::id());
    std::env::set_var(&token_env, "root-token");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
      let mut requests = Vec::new();
      for response in [
        r#"{"data":{"token":"one"}}"#,
        r#"{"data":{"data":{"token":"two"}}}"#,
        r#"{"data":{"cached":true}}"#,
      ] {
        let (mut stream, _) = listener.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        let reply = format!(
          "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
          response.len(),
          response
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
        requests.push(request);
      }
      requests
    });
    let auth = || VaultAuth::Token {
      token_env: token_env.clone(),
    };
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::from([
        (
          "v1".to_owned(),
          SecretProviderConfig::Vault {
            address: address.clone(),
            mount: "secret".to_owned(),
            kv_version: 1,
            namespace: None,
            auth: auth(),
          },
        ),
        (
          "v2".to_owned(),
          SecretProviderConfig::Vault {
            address,
            mount: "secret".to_owned(),
            kv_version: 2,
            namespace: Some("team".to_owned()),
            auth: auth(),
          },
        ),
      ]),
    })
    .unwrap();
    assert!(format!("{session:?}").starts_with("SecretSession { profile:"));
    for (provider, expected) in [("v1", "one"), ("v2", "two")] {
      let value = session
        .resolve(
          &SecretRef {
            provider: provider.to_owned(),
            key: "service/config".to_owned(),
            field: Some("token".to_owned()),
          },
          &CancellationToken::new(),
        )
        .await
        .unwrap();
      assert_eq!(value.into_value(), serde_json::json!(expected));
    }
    let cached_token = session
      .resolve(
        &SecretRef {
          provider: "v1".to_owned(),
          key: "another".to_owned(),
          field: None,
        },
        &CancellationToken::new(),
      )
      .await
      .unwrap();
    assert_eq!(cached_token.into_value(), serde_json::json!({"cached": true}));
    session.shutdown().await;
    std::env::remove_var(token_env);

    let requests = server.await.unwrap();
    assert!(requests[0].starts_with("get /v1/secret/service/config "));
    assert!(requests[0].contains("x-vault-token: root-token"));
    assert!(requests[1].starts_with("get /v1/secret/data/service/config "));
    assert!(requests[1].contains("x-vault-namespace: team"));
    assert!(requests[2].starts_with("get /v1/secret/another "));
  }

  #[tokio::test]
  async fn vault_responses_report_status_shape_json_and_size_errors() {
    let token_env = format!("OCTA_VAULT_ERROR_TOKEN_{}", std::process::id());
    std::env::set_var(&token_env, "root-token");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
      for (status, response) in [
        ("503 Service Unavailable", "{}".to_owned()),
        ("200 OK", "{}".to_owned()),
        ("200 OK", "not-json".to_owned()),
        ("200 OK", "x".repeat(MAX_SECRET_BYTES + 1)),
      ] {
        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = read_request(&mut stream).await;
        let reply = format!(
          "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
          response.len(),
          response
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
      }
    });
    let providers = ["status", "shape", "json", "large"]
      .into_iter()
      .map(|alias| {
        (
          alias.to_owned(),
          SecretProviderConfig::Vault {
            address: address.clone(),
            mount: "secret".to_owned(),
            kv_version: 1,
            namespace: None,
            auth: VaultAuth::Token {
              token_env: token_env.clone(),
            },
          },
        )
      })
      .collect();
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers,
    })
    .unwrap();
    for (provider, expected) in [
      ("status", "status 503"),
      ("shape", "has no data"),
      ("json", "not valid JSON"),
      ("large", "1 MiB limit"),
    ] {
      let error = session
        .resolve(
          &SecretRef {
            provider: provider.to_owned(),
            key: "service".to_owned(),
            field: None,
          },
          &CancellationToken::new(),
        )
        .await
        .unwrap_err();
      assert!(error.to_string().contains(expected), "unexpected error: {error}");
    }
    std::env::remove_var(token_env);
    server.await.unwrap();
  }

  #[tokio::test]
  async fn vault_jwt_tokens_are_renewed_and_revoked() {
    let directory = TempDir::new().unwrap();
    let jwt_path = directory.path().join("jwt");
    fs::write(&jwt_path, "signed-jwt").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
      let mut paths = Vec::new();
      for response in [
        r#"{"auth":{"client_token":"first","lease_duration":1,"renewable":true}}"#,
        r#"{"auth":{"client_token":"renewed","lease_duration":120,"renewable":true}}"#,
        "{}",
      ] {
        let (mut stream, _) = listener.accept().await.unwrap();
        paths.push(read_request_path(&mut stream).await);
        let reply = format!(
          "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
          response.len(),
          response
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
      }
      paths
    });
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::new(),
    })
    .unwrap();
    let auth = VaultAuth::Jwt {
      role: "ci".to_owned(),
      jwt_path,
      mount: "auth/jwt".to_owned(),
    };

    assert_eq!(
      session
        .vault_token("vault", &address, Some("team"), &auth)
        .await
        .unwrap(),
      "first"
    );
    assert_eq!(
      session
        .vault_token("vault", &address, Some("team"), &auth)
        .await
        .unwrap(),
      "renewed"
    );
    session.shutdown().await;

    assert_eq!(
      server.await.unwrap(),
      [
        "/v1/auth/jwt/login",
        "/v1/auth/token/renew-self",
        "/v1/auth/token/revoke-self"
      ]
    );
  }

  #[tokio::test]
  async fn vault_auth_reports_missing_tokens_and_failed_jwt_login() {
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::new(),
    })
    .unwrap();
    let missing_env = format!("OCTA_MISSING_VAULT_TOKEN_{}", std::process::id());
    let error = session
      .vault_token(
        "vault",
        "http://127.0.0.1:1",
        None,
        &VaultAuth::Token { token_env: missing_env },
      )
      .await
      .unwrap_err();
    assert!(error.to_string().contains("token env"));

    let directory = TempDir::new().unwrap();
    let jwt_path = directory.path().join("jwt");
    fs::write(&jwt_path, "signed").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let _ = read_request(&mut stream).await;
      stream
        .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
        .await
        .unwrap();
    });
    let error = session
      .vault_token(
        "vault",
        &address,
        None,
        &VaultAuth::Jwt {
          role: "ci".to_owned(),
          jwt_path,
          mount: "auth/jwt".to_owned(),
        },
      )
      .await
      .unwrap_err();
    assert!(error.to_string().contains("login returned status 403"));
    server.await.unwrap();
  }

  #[tokio::test]
  async fn failed_vault_renewal_falls_back_to_a_fresh_jwt_login() {
    let directory = TempDir::new().unwrap();
    let jwt_path = directory.path().join("jwt");
    fs::write(&jwt_path, "signed").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
      let mut paths = Vec::new();
      for (status, response) in [
        ("503 Service Unavailable", "{}"),
        (
          "200 OK",
          r#"{"auth":{"client_token":"fresh","lease_duration":120,"renewable":false}}"#,
        ),
        ("200 OK", "{}"),
      ] {
        let (mut stream, _) = listener.accept().await.unwrap();
        paths.push(read_request_path(&mut stream).await);
        let reply = format!(
          "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
          response.len(),
          response
        );
        stream.write_all(reply.as_bytes()).await.unwrap();
      }
      paths
    });
    let session = SecretSession::new(SecretProfile {
      version: 1,
      cache_ttl_seconds: 60,
      providers: HashMap::new(),
    })
    .unwrap();
    session.vault_tokens.lock().await.insert(
      "vault".to_owned(),
      VaultToken {
        value: "expired".to_owned(),
        expires: Some(Instant::now()),
        renewable: true,
        owned: true,
        address: address.clone(),
        namespace: Some("team".to_owned()),
      },
    );
    let token = session
      .vault_token(
        "vault",
        &address,
        Some("team"),
        &VaultAuth::Jwt {
          role: "ci".to_owned(),
          jwt_path,
          mount: "auth/jwt".to_owned(),
        },
      )
      .await
      .unwrap();
    assert_eq!(token, "fresh");
    session.shutdown().await;
    assert_eq!(
      server.await.unwrap(),
      [
        "/v1/auth/token/renew-self",
        "/v1/auth/jwt/login",
        "/v1/auth/token/revoke-self"
      ]
    );
  }

  async fn read_request_path(stream: &mut TcpStream) -> String {
    read_request(stream).await.split_whitespace().nth(1).unwrap().to_owned()
  }

  async fn read_request(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
      let read = stream.read(&mut buffer).await.unwrap();
      if read == 0 {
        break;
      }
      bytes.extend_from_slice(&buffer[..read]);
      if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
        let headers = String::from_utf8_lossy(&bytes[..header_end]);
        let content_length = headers
          .lines()
          .find_map(|line| {
            line
              .to_ascii_lowercase()
              .strip_prefix("content-length: ")
              .map(str::to_owned)
          })
          .and_then(|value| value.parse::<usize>().ok())
          .unwrap_or(0);
        if bytes.len() >= header_end + 4 + content_length {
          break;
        }
      }
    }
    String::from_utf8_lossy(&bytes).to_ascii_lowercase()
  }
}
