//! Windows process-tree ownership for commands executed by the shell plugin.
//!
//! `portable-pty` does not expose the spawned process handle needed for direct assignment. The
//! plugin therefore creates a named job, passes its name through a private environment variable,
//! and the isolated Brush child joins it before evaluating user code. Only the plugin retains the
//! owning handle, so dropping `CommandJob` deterministically terminates the complete command tree.

use std::{
  env,
  ffi::{OsStr, OsString},
  io,
  mem::size_of,
  os::windows::{
    ffi::OsStrExt,
    io::{AsRawHandle, FromRawHandle, OwnedHandle},
  },
  ptr,
  sync::atomic::{AtomicU64, Ordering},
};

use windows_sys::Win32::System::{
  JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, OpenJobObjectW,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
  },
  SystemServices::JOB_OBJECT_ASSIGN_PROCESS,
  Threading::GetCurrentProcess,
};

const CHILD_JOB_ENV: &str = "OCTA_WINDOWS_JOB_NAME";
static NEXT_JOB_ID: AtomicU64 = AtomicU64::new(0);

/// Owns the lifetime boundary for one shell command process tree.
pub(crate) struct CommandJob {
  _handle: OwnedHandle,
  name: OsString,
}

impl CommandJob {
  pub(crate) fn create() -> io::Result<Self> {
    let name = OsString::from(format!(
      "octa-shell-command-{}-{}",
      std::process::id(),
      NEXT_JOB_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let handle = create_named(&name)?;
    Ok(Self { _handle: handle, name })
  }

  pub(crate) fn configure_tokio_command(&self, command: &mut tokio::process::Command) {
    command.env(CHILD_JOB_ENV, &self.name);
  }

  pub(crate) fn configure_pty_command(&self, command: &mut portable_pty::CommandBuilder) {
    command.env(CHILD_JOB_ENV, &self.name);
  }

  pub(crate) fn join_inherited() -> io::Result<()> {
    let Some(name) = env::var_os(CHILD_JOB_ENV) else {
      return Ok(());
    };
    env::remove_var(CHILD_JOB_ENV);

    let wide_name = wide_string(&name)?;
    let handle = unsafe { OpenJobObjectW(JOB_OBJECT_ASSIGN_PROCESS, 0, wide_name.as_ptr()) };
    if handle.is_null() {
      return Err(io::Error::last_os_error());
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };
    if unsafe { AssignProcessToJobObject(handle.as_raw_handle().cast(), GetCurrentProcess()) } == 0 {
      return Err(io::Error::last_os_error());
    }
    Ok(())
  }
}

fn create_named(name: &OsStr) -> io::Result<OwnedHandle> {
  let wide_name = wide_string(name)?;
  let handle = unsafe { CreateJobObjectW(ptr::null(), wide_name.as_ptr()) };
  if handle.is_null() {
    return Err(io::Error::last_os_error());
  }
  let handle = unsafe { OwnedHandle::from_raw_handle(handle.cast()) };

  let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
  limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  let configured = unsafe {
    SetInformationJobObject(
      handle.as_raw_handle().cast(),
      JobObjectExtendedLimitInformation,
      ptr::from_ref(&limits).cast(),
      size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    )
  };
  if configured == 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(handle)
}

fn wide_string(value: &OsStr) -> io::Result<Vec<u16>> {
  let mut value = value.encode_wide().collect::<Vec<_>>();
  if value.contains(&0) {
    return Err(io::Error::new(io::ErrorKind::InvalidInput, "job name contains NUL"));
  }
  value.push(0);
  Ok(value)
}
