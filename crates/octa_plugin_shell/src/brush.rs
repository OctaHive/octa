use std::{collections::HashMap, env, ffi::OsStr, path::Path, process::Stdio, time::Duration};

use anyhow::{bail, Context};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell};

const CHILD_MODE_ARG: &str = "--octa-brush-command";

pub(crate) fn pty_command(
  source: &str,
  dir: &Path,
  envs: HashMap<String, String>,
  coreutils_path: &Path,
) -> anyhow::Result<portable_pty::CommandBuilder> {
  let executable = env::current_exe().context("Failed to locate the shell plugin executable")?;
  let path = command_path(&envs, coreutils_path)?;
  let mut command = portable_pty::CommandBuilder::new(executable);
  command.arg(CHILD_MODE_ARG);
  command.arg(source);
  command.cwd(dir);
  for (name, value) in envs {
    command.env(name, value);
  }
  command.env("PATH", path);
  Ok(command)
}

/// Builds an isolated Brush process whose standard streams can be routed through the plugin protocol.
pub(crate) fn command(
  source: &str,
  dir: &Path,
  envs: HashMap<String, String>,
  coreutils_path: &Path,
) -> anyhow::Result<tokio::process::Command> {
  let executable = env::current_exe().context("Failed to locate the shell plugin executable")?;
  let path = command_path(&envs, coreutils_path)?;
  let mut command = tokio::process::Command::new(executable);
  command
    .arg(CHILD_MODE_ARG)
    .arg(source)
    .current_dir(dir)
    .envs(envs)
    .env("PATH", path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
  configure_process(&mut command);
  Ok(command)
}

/// Runs the private child mode used by the plugin server to isolate one shell invocation.
pub(crate) async fn run_child() -> anyhow::Result<Option<u8>> {
  let mut args = env::args_os().skip(1);
  if args.next().as_deref() != Some(OsStr::new(CHILD_MODE_ARG)) {
    return Ok(None);
  }

  let source = args.next().context("Brush command is missing")?;
  if args.next().is_some() {
    bail!("Brush child mode accepts exactly one command");
  }
  let source = source
    .into_string()
    .map_err(|_| anyhow::anyhow!("Brush command is not valid UTF-8"))?;

  #[cfg(windows)]
  crate::windows_job::CommandJob::join_inherited().context("Failed to join the command Job Object")?;

  let mut shell = Shell::builder()
    .default_builtins(BuiltinSet::BashMode)
    .profile(ProfileLoadBehavior::Skip)
    .rc(RcLoadBehavior::Skip)
    .build()
    .await
    .context("Failed to initialize Brush")?;
  let result = shell
    .run_dash_c_command(source)
    .await
    .context("Failed to execute Brush command")?;

  Ok(Some(result.exit_code.into()))
}

fn command_path(envs: &HashMap<String, String>, coreutils_path: &Path) -> anyhow::Result<std::ffi::OsString> {
  let configured = envs
    .iter()
    .find(|(name, _)| path_variable(name))
    .map(|(_, value)| value.as_str());
  let inherited = configured
    .map(OsStr::new)
    .map(OsStr::to_os_string)
    .or_else(|| env::var_os("PATH"));
  let paths = std::iter::once(coreutils_path.to_path_buf()).chain(
    inherited
      .as_deref()
      .filter(|value| !value.is_empty())
      .into_iter()
      .flat_map(env::split_paths),
  );

  env::join_paths(paths).context("Failed to add bundled coreutils to PATH")
}

#[cfg(unix)]
fn path_variable(name: &str) -> bool {
  name == "PATH"
}

#[cfg(windows)]
fn path_variable(name: &str) -> bool {
  name.eq_ignore_ascii_case("PATH")
}

#[cfg(unix)]
fn configure_process(command: &mut tokio::process::Command) {
  command.process_group(0);
}

#[cfg(windows)]
fn configure_process(command: &mut tokio::process::Command) {
  const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;
  const CREATE_NO_WINDOW: u32 = 0x08000000;

  command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(unix)]
pub(crate) fn process_group(child: &tokio::process::Child) -> Option<i32> {
  child.id().map(|id| id as i32)
}

#[cfg(windows)]
pub(crate) fn process_group(_child: &tokio::process::Child) -> Option<i32> {
  None
}

/// Waits for the command leader without allowing its process-group id to be reused before cleanup.
pub(crate) async fn wait_for_command(
  child: &mut tokio::process::Child,
  process_group: Option<i32>,
  cancel_token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<i32> {
  #[cfg(any(target_os = "linux", target_os = "macos"))]
  {
    let pid = child
      .id()
      .ok_or_else(|| std::io::Error::other("command process has no id"))?;
    let mut child_events = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;

    loop {
      if process_exited_without_reaping(pid as i32)? {
        // WNOWAIT keeps the leader as a zombie, reserving its PID while the
        // numeric process-group id is used to terminate surviving descendants.
        terminate_descendants(process_group);
        return Ok(child.wait().await?.code().unwrap_or(-1));
      }

      tokio::select! {
        _ = cancel_token.cancelled() => {
          terminate(child, process_group);
          tokio::time::sleep(Duration::from_millis(100)).await;
          terminate_descendants(process_group);
          let _ = child.wait().await;
          return Ok(-1);
        },
        _ = child_events.recv() => {},
      }
    }
  }

  #[cfg(not(any(target_os = "linux", target_os = "macos")))]
  {
    let code = tokio::select! {
      status = child.wait() => status?.code().unwrap_or(-1),
      _ = cancel_token.cancelled() => {
        terminate(child, process_group);
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = child.kill().await;
        -1
      }
    };
    terminate_descendants(process_group);
    Ok(code)
  }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn process_exited_without_reaping(pid: i32) -> std::io::Result<bool> {
  use std::mem::MaybeUninit;

  use nix::libc;

  let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
  // SAFETY: `info` points to writable storage for siginfo_t. WNOHANG makes
  // waitid non-blocking and WNOWAIT explicitly leaves the child waitable.
  let result = unsafe {
    libc::waitid(
      libc::P_PID,
      pid as libc::id_t,
      info.as_mut_ptr(),
      libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
    )
  };
  if result == -1 {
    return Err(std::io::Error::last_os_error());
  }

  // SAFETY: a successful waitid call initializes siginfo_t; with WNOHANG the
  // zeroed si_pid is retained when the selected child has not exited yet.
  Ok(unsafe { info.assume_init().si_pid() } != 0)
}

#[cfg(unix)]
pub(crate) fn terminate(_child: &mut tokio::process::Child, process_group: Option<i32>) {
  use nix::sys::signal::{kill, Signal};
  use nix::unistd::Pid;

  if let Some(process_group) = process_group {
    let _ = kill(Pid::from_raw(-process_group), Signal::SIGTERM);
  }
}

#[cfg(windows)]
pub(crate) fn terminate(child: &mut tokio::process::Child, _process_group: Option<i32>) {
  let _ = child.start_kill();
}

/// Terminates the command process group, including descendants that outlive its leader.
pub(crate) fn terminate_descendants(process_group: Option<i32>) {
  #[cfg(unix)]
  if let Some(process_group) = process_group {
    use nix::{
      sys::signal::{kill, Signal},
      unistd::Pid,
    };
    let _ = kill(Pid::from_raw(-process_group), Signal::SIGKILL);
  }

  #[cfg(windows)]
  let _ = process_group;
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
  #[test]
  fn wait_without_reaping_rejects_an_unknown_child() {
    assert!(super::process_exited_without_reaping(i32::MAX).is_err());
  }
}
