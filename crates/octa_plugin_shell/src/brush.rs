use std::{collections::HashMap, env, ffi::OsStr, path::Path, process::Stdio};

use anyhow::{bail, Context};
use brush_builtins::{BuiltinSet, ShellBuilderExt};
use brush_core::{ProfileLoadBehavior, RcLoadBehavior, Shell};

const CHILD_MODE_ARG: &str = "--octa-brush-command";

/// Builds an isolated Brush process whose standard streams can be routed through the plugin protocol.
pub(crate) fn command(
  source: &str,
  dir: &Path,
  envs: HashMap<String, String>,
) -> anyhow::Result<tokio::process::Command> {
  let executable = env::current_exe().context("Failed to locate the shell plugin executable")?;
  let mut command = tokio::process::Command::new(executable);
  command
    .arg(CHILD_MODE_ARG)
    .arg(source)
    .current_dir(dir)
    .envs(envs)
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
pub(crate) fn terminate(child: &mut tokio::process::Child) {
  use nix::sys::signal::{kill, Signal};
  use nix::unistd::Pid;

  if let Some(pid) = child.id() {
    let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGTERM);
  }
}

#[cfg(windows)]
pub(crate) fn terminate(child: &mut tokio::process::Child) {
  use windows_sys::Win32::Foundation::CloseHandle;
  use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

  if let Some(pid) = child.id() {
    unsafe {
      let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
      if !handle.is_null() {
        TerminateProcess(handle, 1);
        CloseHandle(handle);
      }
    }
  }
}
