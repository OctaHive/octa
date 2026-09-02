use std::{collections::HashMap, path::Path, time::Duration};

use octa_plugin::protocol::PluginResponse;
use octa_plugin_manager::{
  plugin_client::{PluginExecution, PluginExecutionRequest},
  plugin_manager::PluginManager,
};
use tokio_util::sync::CancellationToken;

fn plugin_manager() -> (PluginManager, String) {
  let executable = Path::new(env!("CARGO_BIN_EXE_octa_plugin_shell"));
  let directory = executable.parent().unwrap();
  let name = executable.file_name().unwrap().to_string_lossy().into_owned();
  (PluginManager::new(directory), name)
}

fn request(command: &str) -> PluginExecutionRequest {
  PluginExecutionRequest {
    params: command.to_owned(),
    dry: false,
    args: Vec::new(),
    dir: std::env::current_dir().unwrap(),
    vars: HashMap::new(),
    envs: HashMap::new(),
    secret_vars: Vec::new(),
    redact_params: false,
  }
}

async fn collect(mut execution: PluginExecution) -> (String, String, i32) {
  let mut stdout = String::new();
  let mut stderr = String::new();
  loop {
    match execution.receive_output(&CancellationToken::new()).await.unwrap() {
      Some(PluginResponse::Stdout { line, .. }) => stdout.push_str(&line),
      Some(PluginResponse::Stderr { line, .. }) => stderr.push_str(&line),
      Some(PluginResponse::ExitStatus { code, .. }) => return (stdout, stderr, code),
      Some(PluginResponse::Error { message, .. }) => panic!("Plugin command failed: {message}"),
      Some(_) => {},
      None => panic!("Plugin response stream closed before ExitStatus"),
    }
  }
}

#[tokio::test]
async fn executes_bash_syntax_through_brush() {
  let (manager, plugin_name) = plugin_manager();
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("shell").await.unwrap();
  let execution = client
    .start_execution(
      request(r#"values=("Hello" "Brush"); printf '%s\n' "${values[*]}""#),
      CancellationToken::new(),
    )
    .await
    .unwrap();

  let (stdout, stderr, code) = collect(execution).await;
  assert_eq!(stdout.trim(), "Hello Brush");
  assert!(stderr.is_empty());
  assert_eq!(code, 0);
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn cancels_an_isolated_brush_process() {
  let (manager, plugin_name) = plugin_manager();
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("shell").await.unwrap();
  let mut execution = client
    .start_execution(request("while true; do :; done"), CancellationToken::new())
    .await
    .unwrap();

  tokio::time::sleep(Duration::from_millis(100)).await;
  tokio::time::timeout(Duration::from_secs(5), execution.cancel_and_wait())
    .await
    .expect("Brush cancellation timed out")
    .unwrap();
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn sources_a_bash_script() {
  let directory = tempfile::tempdir().unwrap();
  std::fs::write(
    directory.path().join("greet.sh"),
    "names=(Hello from Brush)\nprintf '%s\\n' \"${names[*]}\"\n",
  )
  .unwrap();

  let (manager, plugin_name) = plugin_manager();
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("shell").await.unwrap();
  let mut command = request("source ./greet.sh");
  command.dir = directory.path().to_owned();
  let execution = client.start_execution(command, CancellationToken::new()).await.unwrap();

  let (stdout, stderr, code) = collect(execution).await;
  assert_eq!(stdout.trim(), "Hello from Brush");
  assert!(stderr.is_empty());
  assert_eq!(code, 0);
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn isolates_command_input_from_the_plugin_protocol() {
  let (manager, plugin_name) = plugin_manager();
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("shell").await.unwrap();
  let execution = client
    .start_execution(
      request("if read value; then echo unexpected; else echo closed; fi"),
      CancellationToken::new(),
    )
    .await
    .unwrap();

  let (stdout, stderr, code) = tokio::time::timeout(Duration::from_secs(5), collect(execution))
    .await
    .expect("Brush command waited for plugin protocol input");
  assert_eq!(stdout.trim(), "closed");
  assert!(stderr.is_empty());
  assert_eq!(code, 0);
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}

#[tokio::test]
async fn executes_bundled_coreutils_without_system_commands() {
  let directory = tempfile::tempdir().unwrap();
  let large_input = vec![b'x'; 256 * 1024];
  std::fs::write(directory.path().join("large.bin"), &large_input).unwrap();
  let (manager, plugin_name) = plugin_manager();
  manager.start_plugin(&plugin_name).await.unwrap();
  let client = manager.get_client("shell").await.unwrap();
  let mut command = request(
    r#"cat large.bin | base64 > large.b64
dir=$(mktemp -d)
mkdir -p "$dir/nested"
printf Octa > "$dir/source"
cp "$dir/source" "$dir/copied"
mv "$dir/copied" "$dir/moved"
touch "$dir/touched"
cat "$dir/moved"
printf '\n'
printf Octa | base64
ls "$dir"
sleep 0.01
rm -r "$dir""#,
  );
  command.dir = directory.path().to_owned();
  command.envs.insert("PATH".to_owned(), String::new());
  let execution = client.start_execution(command, CancellationToken::new()).await.unwrap();

  let (stdout, stderr, code) = tokio::time::timeout(Duration::from_secs(10), collect(execution))
    .await
    .expect("bundled coreutils pipeline timed out");
  assert!(stdout.lines().any(|line| line == "Octa"));
  assert!(stdout.lines().any(|line| line == "T2N0YQ=="));
  assert!(stdout.lines().any(|line| line == "moved"));
  assert!(stdout.lines().any(|line| line == "touched"));
  assert!(stderr.is_empty(), "{stderr}");
  assert_eq!(code, 0);
  assert!(std::fs::metadata(directory.path().join("large.b64")).unwrap().len() > large_input.len() as u64);
  assert!(manager.shutdown_all().await.into_iter().all(|result| result.is_ok()));
}
