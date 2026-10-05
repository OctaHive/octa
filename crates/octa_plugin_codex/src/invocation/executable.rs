//! Operator-owned Codex executable selection and compatibility probing.
//!
//! Task configuration never reaches this module. The operator must select one
//! absolute file through [`CODEX_EXECUTABLE_ENV`]; the resolver canonicalizes
//! that file and starts it directly for a bounded `--version` probe. It never
//! searches `PATH`, invokes a shell, or falls back to parsing terminal output.

use std::{
  ffi::OsString,
  io,
  io::Read,
  path::{Path, PathBuf},
  process::Stdio,
  time::Duration,
};

#[cfg(windows)]
use std::ffi::OsStr;

use anyhow::{bail, Context};
use semver::Version;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::PLATFORM_ENVIRONMENT;
use crate::process::{self, CapturedStream, ProbeOutcome};

/// Plugin-process environment entry controlled by the local operator or job
/// image. It is intentionally not part of the task schema or command payload.
pub(crate) const CODEX_EXECUTABLE_ENV: &str = "OCTA_CODEX_EXECUTABLE";

const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_VERSION_STREAM_BYTES: usize = 4 * 1024;
const VERSION_PRODUCT: &str = "codex-cli";
/// Exact Codex CLI releases whose JSONL contract is covered by fixtures.
const SUPPORTED_CODEX_VERSIONS: &[&str] = &["0.130.0"];

/// One exact executable whose machine-readable contract is supported.
///
/// Fields are private so production code cannot manufacture compatibility
/// evidence. The retained fingerprint binds the observed version to one file;
/// callers can only prepare a command after that identity is revalidated.
pub(crate) struct CodexExecutable {
  path: PathBuf,
  #[cfg_attr(
    not(test),
    expect(dead_code, reason = "task 6.2 records the observed version in provenance")
  )]
  version: Version,
  #[cfg_attr(
    not(test),
    expect(dead_code, reason = "task 5.1 revalidates this identity at the task spawn boundary")
  )]
  fingerprint: ExecutableFingerprint,
}

impl CodexExecutable {
  /// Resolves and probes the executable selected in the plugin process's
  /// operator-owned environment.
  pub(crate) async fn resolve_from_operator_environment(cancellation: &CancellationToken) -> anyhow::Result<Self> {
    Self::resolve_selected(std::env::var_os(CODEX_EXECUTABLE_ENV), cancellation).await
  }

  async fn resolve_selected(selection: Option<OsString>, cancellation: &CancellationToken) -> anyhow::Result<Self> {
    let Some(selection) = selection.filter(|value| !value.is_empty()) else {
      bail!("Codex executable is not configured; the operator must set {CODEX_EXECUTABLE_ENV}");
    };
    let selected = PathBuf::from(selection);
    if !selected.is_absolute() {
      bail!("operator-selected Codex executable path must be absolute");
    }

    let path = match tokio::fs::canonicalize(&selected).await {
      Ok(path) => path,
      Err(error) if error.kind() == io::ErrorKind::NotFound => {
        bail!("operator-selected Codex executable does not exist")
      },
      Err(_) => bail!("operator-selected Codex executable cannot be resolved"),
    };
    let fingerprint = fingerprint_executable(path.clone()).await?;

    let version = probe_version(&path, &fingerprint, cancellation).await?;
    fingerprint.ensure_unchanged(&path).await?;
    if !version_is_supported(&version) {
      let supported = SUPPORTED_CODEX_VERSIONS.join(", ");
      bail!("Codex CLI version {version} is not supported; supported versions: {supported}");
    }
    Ok(Self {
      path,
      version,
      fingerprint,
    })
  }

  /// Canonical executable path recorded in future provenance.
  #[cfg_attr(
    not(test),
    expect(dead_code, reason = "task 6.2 records the executable identity in provenance")
  )]
  pub(crate) fn path(&self) -> &Path {
    &self.path
  }

  /// Validated Codex CLI release observed during the compatibility probe.
  #[cfg_attr(
    not(test),
    expect(dead_code, reason = "task 6.2 records the observed version in provenance")
  )]
  pub(crate) fn version(&self) -> String {
    self.version.to_string()
  }

  /// Provides non-spawning compatibility evidence to invocation unit tests.
  /// Production construction remains restricted to the resolver above.
  #[cfg(test)]
  pub(super) fn fixture() -> Self {
    let path = std::env::current_exe().expect("test executable path must be available");
    Self {
      fingerprint: ExecutableFingerprint::read(&path).expect("test executable must be fingerprintable"),
      path,
      version: Version::parse(SUPPORTED_CODEX_VERSIONS[0]).expect("supported test version must be valid"),
    }
  }
}

struct ExecutableFingerprint {
  handle: same_file::Handle,
  size: u64,
  digest: blake3::Hash,
}

impl ExecutableFingerprint {
  fn read(path: &Path) -> anyhow::Result<Self> {
    let mut file = std::fs::File::open(path).context("failed to open the operator-selected Codex executable")?;
    let metadata = file
      .metadata()
      .context("failed to inspect the operator-selected Codex executable")?;
    if !metadata.is_file() {
      bail!("operator-selected Codex executable must be a regular file");
    }
    ensure_directly_executable(path, &metadata)?;

    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
      let read = file
        .read(&mut buffer)
        .context("failed to fingerprint the operator-selected Codex executable")?;
      if read == 0 {
        break;
      }
      hasher.update(&buffer[..read]);
    }
    let handle =
      same_file::Handle::from_file(file).context("failed to identify the operator-selected Codex executable")?;
    Ok(Self {
      handle,
      size: metadata.len(),
      digest: hasher.finalize(),
    })
  }

  async fn ensure_unchanged(&self, path: &Path) -> anyhow::Result<()> {
    let current = fingerprint_executable(path.to_owned()).await;
    if current
      .is_ok_and(|current| self.handle == current.handle && self.size == current.size && self.digest == current.digest)
    {
      return Ok(());
    }
    bail!("operator-selected Codex executable changed after compatibility validation")
  }
}

async fn fingerprint_executable(path: PathBuf) -> anyhow::Result<ExecutableFingerprint> {
  tokio::task::spawn_blocking(move || ExecutableFingerprint::read(&path))
    .await
    .context("Codex executable fingerprint task failed")?
}

#[cfg(unix)]
fn ensure_directly_executable(_path: &Path, metadata: &std::fs::Metadata) -> anyhow::Result<()> {
  use std::os::unix::fs::PermissionsExt;

  if metadata.permissions().mode() & 0o111 == 0 {
    bail!("operator-selected Codex executable is not executable");
  }
  Ok(())
}

#[cfg(windows)]
fn ensure_directly_executable(path: &Path, _metadata: &std::fs::Metadata) -> anyhow::Result<()> {
  // Rust may route batch files through cmd.exe on Windows. Requiring a native
  // image preserves the no-shell contract and avoids command-line re-parsing.
  if !path
    .extension()
    .and_then(OsStr::to_str)
    .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
  {
    bail!("operator-selected Codex executable must be a native .exe file on Windows");
  }
  Ok(())
}

#[cfg(not(any(unix, windows)))]
fn ensure_directly_executable(_path: &Path, _metadata: &std::fs::Metadata) -> anyhow::Result<()> {
  Ok(())
}

async fn probe_version(
  path: &Path,
  fingerprint: &ExecutableFingerprint,
  cancellation: &CancellationToken,
) -> anyhow::Result<Version> {
  fingerprint.ensure_unchanged(path).await?;
  let mut command = Command::new(path);
  command
    .arg("--version")
    .env_clear()
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
  if let Some(parent) = path.parent() {
    command.current_dir(parent);
  }
  for name in PLATFORM_ENVIRONMENT {
    if let Some(value) = std::env::var_os(name) {
      command.env(name, value);
    }
  }

  let outcome = process::run_version_probe(command, VERSION_PROBE_TIMEOUT, MAX_VERSION_STREAM_BYTES, cancellation)
    .await
    .map_err(|_| anyhow::anyhow!("operator-selected Codex executable could not be started or supervised"))?;
  let (status, stdout, stderr) = match outcome {
    ProbeOutcome::Completed { status, stdout, stderr } => (status, stdout, stderr),
    ProbeOutcome::Cancelled => bail!("Codex version probe was cancelled"),
    ProbeOutcome::TimedOut => bail!(
      "Codex version probe exceeded its {} second limit",
      VERSION_PROBE_TIMEOUT.as_secs()
    ),
  };
  let stdout = complete_stream(stdout)?;
  let _stderr = complete_stream(stderr)?;
  if !status.success() {
    bail!("Codex version probe exited unsuccessfully");
  }
  parse_version_output(&stdout)
}

fn complete_stream(stream: CapturedStream) -> anyhow::Result<Vec<u8>> {
  match stream {
    CapturedStream::Complete(bytes) => Ok(bytes),
    CapturedStream::Exceeded => bail!("Codex version probe output exceeded {MAX_VERSION_STREAM_BYTES} bytes"),
  }
}

fn parse_version_output(output: &[u8]) -> anyhow::Result<Version> {
  let output = std::str::from_utf8(output).map_err(|_| anyhow::anyhow!("Codex version output is not valid UTF-8"))?;
  let output = output.strip_suffix('\n').unwrap_or(output);
  let output = output.strip_suffix('\r').unwrap_or(output);
  let Some((product, version)) = output.split_once(' ') else {
    bail!("malformed Codex version output; expected 'codex-cli <major>.<minor>.<patch>'");
  };
  if product != VERSION_PRODUCT || version.is_empty() || version.bytes().any(|byte| byte.is_ascii_whitespace()) {
    bail!("malformed Codex version output; expected 'codex-cli <major>.<minor>.<patch>'");
  }
  let version = Version::parse(version)
    .map_err(|_| anyhow::anyhow!("malformed Codex version output; expected a semantic version"))?;
  if !version.pre.is_empty() || !version.build.is_empty() {
    bail!("malformed Codex version output; prerelease and build metadata are not supported");
  }
  Ok(version)
}

fn version_is_supported(version: &Version) -> bool {
  SUPPORTED_CODEX_VERSIONS
    .iter()
    .filter_map(|supported| Version::parse(supported).ok())
    .any(|supported| supported == *version)
}

#[cfg(test)]
#[path = "executable_tests.rs"]
mod tests;
