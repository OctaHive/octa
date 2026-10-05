//! Ownership of operating-system processes started by the Codex plugin.
//!
//! Even the compatibility probe executes untrusted external code: it can
//! spawn descendants, retain inherited pipes, or ignore cancellation. This
//! module is therefore the only place that spawns Codex processes. Unix uses
//! a dedicated process group and keeps the leader waitable until descendants
//! are terminated. Windows assigns the child to a kill-on-close Job Object.

use std::{io, process::ExitStatus, time::Duration};

use tokio::{
  io::{AsyncRead, AsyncReadExt},
  process::{Child, Command},
  task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// Captured bytes from one bounded probe stream.
pub(crate) enum CapturedStream {
  /// The stream ended within its configured limit.
  Complete(Vec<u8>),
  /// The stream contained at least one byte beyond its configured limit.
  Exceeded,
}

/// Terminal outcome of one compatibility probe.
pub(crate) enum ProbeOutcome {
  /// The leader exited and both bounded streams were collected.
  Completed {
    /// Exit status returned by the probe leader.
    status: ExitStatus,
    /// Bounded standard output.
    stdout: CapturedStream,
    /// Bounded standard error.
    stderr: CapturedStream,
  },
  /// The caller cancelled the probe and the complete tree was terminated.
  Cancelled,
  /// The probe exceeded its private compatibility deadline.
  TimedOut,
}

/// Runs `command` under complete process-tree ownership and captures two
/// independently bounded streams.
pub(crate) async fn run_version_probe(
  mut command: Command,
  timeout: Duration,
  stream_limit: usize,
  cancellation: &CancellationToken,
) -> io::Result<ProbeOutcome> {
  let mut tree = ProcessTree::spawn(&mut command)?;
  let stdout = tree
    .child
    .stdout
    .take()
    .ok_or_else(|| io::Error::other("Codex version probe did not expose stdout"))?;
  let stderr = tree
    .child
    .stderr
    .take()
    .ok_or_else(|| io::Error::other("Codex version probe did not expose stderr"))?;
  let stdout = tokio::spawn(read_bounded(stdout, stream_limit));
  let stderr = tokio::spawn(read_bounded(stderr, stream_limit));

  let deadline = tokio::time::sleep(timeout);
  tokio::pin!(deadline);
  let terminal = tokio::select! {
    biased;
    _ = cancellation.cancelled() => ProbeTerminal::Cancelled,
    _ = &mut deadline => ProbeTerminal::TimedOut,
    status = tree.wait() => ProbeTerminal::Exited(status),
  };

  let outcome = match terminal {
    ProbeTerminal::Exited(Ok(status)) => {
      // Await both readers before propagating either failure. Dropping the
      // second JoinHandle after an early `?` would detach a live task.
      let (stdout, stderr) = tokio::join!(join_reader(stdout), join_reader(stderr));
      ProbeOutcome::Completed {
        status,
        stdout: stdout?,
        stderr: stderr?,
      }
    },
    ProbeTerminal::Exited(Err(error)) => {
      let _ = terminate_and_drain(&mut tree, stdout, stderr).await;
      return Err(error);
    },
    ProbeTerminal::Cancelled => {
      terminate_and_drain(&mut tree, stdout, stderr).await?;
      ProbeOutcome::Cancelled
    },
    ProbeTerminal::TimedOut => {
      terminate_and_drain(&mut tree, stdout, stderr).await?;
      ProbeOutcome::TimedOut
    },
  };
  Ok(outcome)
}

enum ProbeTerminal {
  Exited(io::Result<ExitStatus>),
  Cancelled,
  TimedOut,
}

async fn join_reader(reader: JoinHandle<io::Result<CapturedStream>>) -> io::Result<CapturedStream> {
  reader
    .await
    .map_err(|error| io::Error::other(format!("probe reader task failed: {error}")))?
}

async fn terminate_and_drain(
  tree: &mut ProcessTree,
  stdout: JoinHandle<io::Result<CapturedStream>>,
  stderr: JoinHandle<io::Result<CapturedStream>>,
) -> io::Result<()> {
  // Closing the tree must make both pipes finite. Always join both tasks even
  // when termination itself fails, otherwise a reader could escape detached.
  let termination = tree.terminate().await;
  let _ = tokio::join!(join_reader(stdout), join_reader(stderr));
  termination
}

async fn read_bounded(reader: impl AsyncRead + Unpin, limit: usize) -> io::Result<CapturedStream> {
  let mut bytes = Vec::with_capacity(limit);
  reader
    .take((limit as u64).saturating_add(1))
    .read_to_end(&mut bytes)
    .await?;
  if bytes.len() > limit {
    Ok(CapturedStream::Exceeded)
  } else {
    Ok(CapturedStream::Complete(bytes))
  }
}

struct ProcessTree {
  child: Child,
  #[cfg(unix)]
  process_group: i32,
  #[cfg(windows)]
  job: Option<std::os::windows::io::OwnedHandle>,
}

impl ProcessTree {
  fn spawn(command: &mut Command) -> io::Result<Self> {
    command.kill_on_drop(true);
    configure_process(command);
    let child = command.spawn()?;

    #[cfg(unix)]
    {
      let process_group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .ok_or_else(|| io::Error::other("Codex version probe has no valid process id"))?;
      Ok(Self { child, process_group })
    }

    #[cfg(windows)]
    {
      let mut child = child;
      match assign_kill_on_close_job(&child).and_then(|job| {
        resume_primary_thread(&child)?;
        Ok(job)
      }) {
        Ok(job) => Ok(Self { child, job: Some(job) }),
        Err(error) => {
          let _ = child.start_kill();
          Err(error)
        },
      }
    }

    #[cfg(not(any(unix, windows)))]
    {
      Ok(Self { child })
    }
  }

  async fn wait(&mut self) -> io::Result<ExitStatus> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
      let pid = self.process_group;
      let mut child_events = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
      loop {
        if process_exited_without_reaping(pid)? {
          kill_process_group(pid);
          return self.child.wait().await;
        }
        child_events.recv().await;
      }
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
      let status = self.child.wait().await;
      kill_process_group(self.process_group);
      status
    }

    #[cfg(windows)]
    {
      let status = self.child.wait().await;
      // Closing the Job Object after the leader exits terminates descendants
      // that otherwise could retain the probe pipes indefinitely.
      self.job.take();
      status
    }

    #[cfg(not(any(unix, windows)))]
    {
      self.child.wait().await
    }
  }

  async fn terminate(&mut self) -> io::Result<()> {
    #[cfg(unix)]
    kill_process_group(self.process_group);

    #[cfg(windows)]
    {
      self.job.take();
    }

    #[cfg(not(any(unix, windows)))]
    {
      let _ = self.child.start_kill();
    }

    self.child.wait().await.map(|_| ())
  }
}

impl Drop for ProcessTree {
  fn drop(&mut self) {
    #[cfg(unix)]
    kill_process_group(self.process_group);

    #[cfg(windows)]
    {
      self.job.take();
    }

    let _ = self.child.start_kill();
  }
}

#[cfg(unix)]
fn configure_process(command: &mut Command) {
  command.process_group(0);
}

#[cfg(windows)]
fn configure_process(command: &mut Command) {
  use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED};

  // Suspending the initial thread closes the otherwise unavoidable window in
  // which Codex could create an unowned descendant before Job assignment.
  command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
fn configure_process(_command: &mut Command) {}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn process_exited_without_reaping(pid: i32) -> io::Result<bool> {
  use std::mem::MaybeUninit;

  let mut info = MaybeUninit::<libc::siginfo_t>::zeroed();
  // SAFETY: `info` is writable storage for `siginfo_t`; WNOHANG keeps this
  // check non-blocking and WNOWAIT deliberately preserves the leader until
  // the process group has been terminated.
  let result = unsafe {
    libc::waitid(
      libc::P_PID,
      pid as libc::id_t,
      info.as_mut_ptr(),
      libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
    )
  };
  if result == -1 {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: successful `waitid` initializes `siginfo_t`; a zero pid means
  // that the selected child has not exited yet under WNOHANG.
  Ok(unsafe { info.assume_init().si_pid() } != 0)
}

#[cfg(unix)]
fn kill_process_group(process_group: i32) {
  // SAFETY: a negative pid addresses the process group created exclusively
  // for this probe. Failure is intentionally ignored during best-effort Drop.
  unsafe {
    libc::kill(-process_group, libc::SIGKILL);
  }
}

#[cfg(windows)]
fn assign_kill_on_close_job(child: &Child) -> io::Result<std::os::windows::io::OwnedHandle> {
  use std::{
    mem::size_of,
    os::windows::io::{FromRawHandle, OwnedHandle},
    ptr,
  };

  use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
  };

  let raw_job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
  if raw_job.is_null() {
    return Err(io::Error::last_os_error());
  }
  let job = unsafe { OwnedHandle::from_raw_handle(raw_job.cast()) };
  let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
  limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
  let configured = unsafe {
    SetInformationJobObject(
      raw_job,
      JobObjectExtendedLimitInformation,
      ptr::from_ref(&limits).cast(),
      size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
    )
  };
  if configured == 0 {
    return Err(io::Error::last_os_error());
  }
  let process = child
    .raw_handle()
    .ok_or_else(|| io::Error::other("Codex version probe exited before Job Object assignment"))?;
  if unsafe { AssignProcessToJobObject(raw_job, process.cast()) } == 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(job)
}

#[cfg(windows)]
fn resume_primary_thread(child: &Child) -> io::Result<()> {
  use std::{
    mem::size_of,
    os::windows::io::{FromRawHandle, OwnedHandle},
  };

  use windows_sys::Win32::{
    Foundation::INVALID_HANDLE_VALUE,
    System::{
      Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
      },
      Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
    },
  };

  let process_id = child
    .id()
    .ok_or_else(|| io::Error::other("suspended Codex version probe has no process id"))?;
  let raw_snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
  if raw_snapshot == INVALID_HANDLE_VALUE {
    return Err(io::Error::last_os_error());
  }
  // SAFETY: a successful snapshot call returns an owned HANDLE which is
  // closed exactly once by `OwnedHandle`.
  let snapshot = unsafe { OwnedHandle::from_raw_handle(raw_snapshot.cast()) };
  let mut entry = THREADENTRY32 {
    dwSize: size_of::<THREADENTRY32>() as u32,
    ..THREADENTRY32::default()
  };
  let mut present = unsafe { Thread32First(raw_snapshot, &mut entry) } != 0;
  while present {
    if entry.th32OwnerProcessID == process_id {
      let raw_thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
      if raw_thread.is_null() {
        return Err(io::Error::last_os_error());
      }
      // SAFETY: `OpenThread` returned an owned HANDLE which remains live for
      // the resume call and is then closed exactly once.
      let thread = unsafe { OwnedHandle::from_raw_handle(raw_thread.cast()) };
      if unsafe { ResumeThread(raw_thread) } == u32::MAX {
        return Err(io::Error::last_os_error());
      }
      drop(thread);
      drop(snapshot);
      return Ok(());
    }
    present = unsafe { Thread32Next(raw_snapshot, &mut entry) } != 0;
  }
  Err(io::Error::other(
    "suspended Codex version probe has no discoverable primary thread",
  ))
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
