//! Cross-process proof that runner protocol v2 shares results through HTTPS.

use std::{
  env, fs,
  net::SocketAddr,
  path::{Path, PathBuf},
  sync::mpsc,
  thread,
  time::Duration,
};

use assert_cmd::Command;
use octa_cache::CacheStore as _;
use octa_cache_http::{HttpCacheConfig, HttpCacheStore};
use octa_cache_protocol::Digest;
use octa_cache_test_support::ReferenceCache;
use octa_runner_protocol::RUNNER_PROTOCOL_VERSION;
use serde_json::{json, Value};
use tempfile::TempDir;

struct HttpsCache {
  address: SocketAddr,
  state: ReferenceCache,
  handle: axum_server::Handle<SocketAddr>,
  thread: Option<thread::JoinHandle<()>>,
}

impl HttpsCache {
  fn start() -> Self {
    let state = ReferenceCache::new("fixture-token");
    let app = state.router();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let handle = axum_server::Handle::new();
    let server_handle = handle.clone();
    let (ready_tx, ready_rx) = mpsc::sync_channel(0);
    let certificate = fixture_path("cache-cert.pem");
    let key = fixture_path("cache-key.pem");
    let thread = thread::spawn(move || {
      let _ = rustls::crypto::ring::default_provider().install_default();
      tokio::runtime::Runtime::new().unwrap().block_on(async move {
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(certificate, key)
          .await
          .unwrap();
        ready_tx.send(()).unwrap();
        axum_server::from_tcp_rustls(listener, tls)
          .unwrap()
          .handle(server_handle)
          .serve(app.into_make_service())
          .await
          .unwrap();
      });
    });
    ready_rx.recv().unwrap();
    Self {
      address,
      state,
      handle,
      thread: Some(thread),
    }
  }

  fn endpoint(&self) -> String {
    format!("https://127.0.0.1:{}/", self.address.port())
  }
}

impl Drop for HttpsCache {
  fn drop(&mut self) {
    self.handle.graceful_shutdown(Some(Duration::from_secs(1)));
    if let Some(thread) = self.thread.take() {
      thread.join().unwrap();
    }
  }
}

fn fixture_path(name: &str) -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests/fixtures")
    .join(name)
}

fn plugins_dir() -> PathBuf {
  if let Some(path) = env::var_os("OCTA_E2E_PLUGINS_DIR") {
    return PathBuf::from(path);
  }
  let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
  let target = workspace.join("target/debug");
  #[cfg(windows)]
  let plugins = ["octa_plugin_shell.exe", "octa_plugin_tpl.exe"];
  #[cfg(not(windows))]
  let plugins = ["octa_plugin_shell", "octa_plugin_tpl"];
  if plugins.iter().all(|plugin| target.join(plugin).is_file()) {
    target
  } else {
    workspace.join("plugins")
  }
}

fn only_result_blob(root: &Path) -> PathBuf {
  let mut directories = vec![root.to_owned()];
  let mut files = Vec::new();
  while let Some(directory) = directories.pop() {
    for entry in fs::read_dir(directory).unwrap() {
      let entry = entry.unwrap();
      let file_type = entry.file_type().unwrap();
      if file_type.is_dir() {
        directories.push(entry.path());
      } else if file_type.is_file()
        && entry
          .file_name()
          .to_str()
          .is_some_and(|name| name.contains("-zstd-v1-"))
      {
        files.push(entry.path());
      }
    }
  }
  // The local CAS may also contain identity-encoded input-digest memo data.
  // This fixture publishes exactly one compressed task-result bundle.
  assert_eq!(files.len(), 1, "fixture must publish exactly one result blob");
  files.pop().unwrap()
}

fn request(workspace: &TempDir, local: &Path, server: &HttpsCache, token: &Path) -> String {
  let runtime = json!({
    "kind": "native",
    "os": match env::consts::OS { "macos" => "macos", "windows" => "windows", _ => "linux" },
    "architecture": if env::consts::ARCH == "aarch64" { "arm64" } else { "amd64" },
    "environment": Digest::blake3(b"remote-runner-test-environment")
  });
  format!(
    "{}\n",
    json!({
      "type": "start",
      "protocol_version": RUNNER_PROTOCOL_VERSION,
      "request_id": "remote-cache-run",
      "request": {
        "workspace": workspace.path(),
        "data_dir": workspace.path().join("state"),
        "plugins_dir": plugins_dir(),
        "commands": ["build"],
        "cache": {
          "mode": "read_write",
          "namespace": "tests/remote-runner",
          "local_directory": local,
          "runtime": runtime,
          "remote": {
            "endpoint": server.endpoint(),
            "token_file": token,
            "ca_certificate_file": fixture_path("cache-ca.pem"),
            "request_timeout_seconds": 10,
            "max_parallel_transfers": 2
          }
        }
      }
    })
  )
}

#[test]
fn two_runner_processes_share_a_result_over_https() {
  let server = HttpsCache::start();
  let credentials = TempDir::new().unwrap();
  let token = credentials.path().join("token");
  fs::write(&token, "fixture-token\n").unwrap();
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt as _;
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
  }
  let direct = HttpCacheConfig::from_token_file(&server.endpoint(), &token)
    .unwrap()
    .with_ca_certificate_file(Some(&fixture_path("cache-ca.pem")))
    .unwrap();
  tokio::runtime::Runtime::new()
    .unwrap()
    .block_on(
      HttpCacheStore::new(direct, tokio_util::sync::CancellationToken::new())
        .unwrap()
        .get_action("readiness", &Digest::blake3(b"readiness")),
    )
    .unwrap();
  let first = TempDir::new().unwrap();
  let second = TempDir::new().unwrap();
  for workspace in [&first, &second] {
    fs::write(
      workspace.path().join("Octafile.yml"),
      "version: 1\ntasks:\n  build:\n    files:\n      inputs: [input.txt]\n      outputs: [output.txt]\n    cache: {}\n    shell: echo generated > output.txt && echo run >> runs.txt\n",
    )
    .unwrap();
    fs::write(workspace.path().join("input.txt"), "input").unwrap();
  }

  let execute = |workspace: &TempDir, local: &TempDir| {
    Command::cargo_bin("octa-runner")
      .unwrap()
      .write_stdin(request(workspace, local.path(), &server, &token))
      .output()
      .unwrap()
  };
  let first_cache = TempDir::new().unwrap();
  let second_cache = TempDir::new().unwrap();
  let first_output = execute(&first, &first_cache);
  assert!(
    first_output.status.success(),
    "{}",
    String::from_utf8_lossy(&first_output.stdout)
  );
  assert_eq!(
    server.state.blob_count(),
    1,
    "remote blob was not published; stderr={}",
    String::from_utf8_lossy(&first_output.stderr)
  );
  assert_eq!(server.state.action_count(), 1, "remote action was not published");
  let second_output = execute(&second, &second_cache);
  assert!(
    second_output.status.success(),
    "{}",
    String::from_utf8_lossy(&second_output.stdout)
  );

  let outcome = |bytes: &[u8]| {
    bytes
      .split(|byte| *byte == b'\n')
      .filter(|line| !line.is_empty())
      .map(|line| serde_json::from_slice::<Value>(line).unwrap())
      .find(|message| message["type"] == "finished")
      .unwrap()["results"][0]["tasks"]
      .as_array()
      .unwrap()
      .iter()
      .find_map(|task| task.get("cache").cloned())
      .unwrap()
  };
  assert_eq!(outcome(&first_output.stdout)["status"], "miss");
  let second_outcome = outcome(&second_output.stdout);
  assert_eq!(
    second_outcome["status"],
    "hit",
    "first={}, second={}",
    String::from_utf8_lossy(&first_output.stdout),
    String::from_utf8_lossy(&second_output.stdout)
  );
  assert_eq!(second_outcome["layer"], "remote");
  assert_eq!(
    fs::read_to_string(second.path().join("output.txt")).unwrap(),
    "generated\n"
  );
  assert!(!second.path().join("runs.txt").exists());

  // A same-sized L1 corruption is invisible to metadata-only lookup. The
  // expanded bundle verifier must quarantine it and retry the independent L2
  // copy before allowing the task body to execute.
  let local_blob = only_result_blob(&second_cache.path().join("v1/blobs"));
  let size = fs::metadata(&local_blob).unwrap().len() as usize;
  fs::write(&local_blob, vec![b'x'; size]).unwrap();
  fs::remove_file(second.path().join("output.txt")).unwrap();
  let recovered_output = execute(&second, &second_cache);
  assert!(
    recovered_output.status.success(),
    "{}",
    String::from_utf8_lossy(&recovered_output.stdout)
  );
  let recovered_outcome = outcome(&recovered_output.stdout);
  assert_eq!(recovered_outcome["status"], "hit");
  // The action metadata was served by this runner's L1. Blob repair has its
  // own provenance and must not rewrite the action-result provenance.
  assert_eq!(recovered_outcome["layer"], "local");
  assert_eq!(
    fs::read_to_string(second.path().join("output.txt")).unwrap(),
    "generated\n"
  );
  assert!(!second.path().join("runs.txt").exists());
}
