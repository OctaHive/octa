//! Ownership of operating-system processes started by the Codex plugin.
//!
//! Even the compatibility probe executes untrusted external code: it can
//! spawn descendants, retain inherited pipes, or ignore cancellation. This
//! module is therefore the only place that spawns Codex processes. Unix uses
//! a dedicated process group and keeps the leader waitable until descendants
//! are terminated. Windows assigns the child to a kill-on-close Job Object.

use std::{io, process::ExitStatus, process::Stdio, time::Duration};

use tokio::{
  io::{AsyncRead, AsyncReadExt},
  process::{Child, ChildStderr, ChildStdin, ChildStdout, Command},
  task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

/// Bounds forceful cleanup after a compatibility probe is cancelled or times out.
const PROBE_TERMINATION_TIMEOUT: Duration = Duration::from_secs(2);
/// Bounds pipe collection after the owned process boundary has been closed.
const PROBE_DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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
  // The compatibility probe has no input. Taking and dropping the pipe makes
  // EOF observable immediately even when the selected executable reads stdin.
  drop(tree.take_stdin()?);
  let stdout = tree.take_stdout()?;
  let stderr = tree.take_stderr()?;
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
      let (stdout, stderr) = join_readers_bounded(stdout, stderr, PROBE_DRAIN_TIMEOUT).await?;
      ProbeOutcome::Completed { status, stdout, stderr }
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

async fn join_reader(reader: &mut JoinHandle<io::Result<CapturedStream>>) -> io::Result<CapturedStream> {
  (&mut *reader)
    .await
    .map_err(|error| io::Error::other(format!("probe reader task failed: {error}")))?
}

async fn join_readers_bounded(
  mut stdout: JoinHandle<io::Result<CapturedStream>>,
  mut stderr: JoinHandle<io::Result<CapturedStream>>,
  timeout: Duration,
) -> io::Result<(CapturedStream, CapturedStream)> {
  let joined = async {
    // Join both before propagating either error. An early return would detach
    // the other reader and could keep inherited pipe handles alive unnoticed.
    let (stdout, stderr) = tokio::join!(join_reader(&mut stdout), join_reader(&mut stderr));
    Ok((stdout?, stderr?))
  };
  match tokio::time::timeout(timeout, joined).await {
    Ok(result) => result,
    Err(_) => {
      stdout.abort();
      stderr.abort();
      let _ = tokio::join!(&mut stdout, &mut stderr);
      Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "Codex compatibility output pipes did not close after process termination",
      ))
    },
  }
}

async fn terminate_and_drain(
  tree: &mut ProcessTree,
  stdout: JoinHandle<io::Result<CapturedStream>>,
  stderr: JoinHandle<io::Result<CapturedStream>>,
) -> io::Result<()> {
  // Always bound and join both operations. A malicious descendant may retain
  // inherited pipe handles even after escaping the owned process boundary.
  let termination = tokio::time::timeout(PROBE_TERMINATION_TIMEOUT, tree.terminate())
    .await
    .unwrap_or_else(|_| {
      Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "timed out while terminating the Codex compatibility probe",
      ))
    });
  let drain = join_readers_bounded(stdout, stderr, PROBE_DRAIN_TIMEOUT).await;
  termination?;
  drain.map(|_| ())
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

/// Direct child plus the operating-system boundary that owns its descendants.
///
/// All handles are piped here rather than at call sites so every Codex launch
/// has the same isolation and I/O contract. Dropping this value closes the
/// Windows Job Object or kills the Unix process group before killing the
/// leader, preventing a descendant from retaining pipes or continuing work.
pub(crate) struct ProcessTree {
  child: Child,
  #[cfg(unix)]
  process_group: Option<i32>,
  #[cfg(any(target_os = "linux", target_os = "macos"))]
  child_events: tokio::signal::unix::Signal,
  #[cfg(windows)]
  job: Option<std::os::windows::io::OwnedHandle>,
}

impl ProcessTree {
  /// Spawns one directly selected executable without involving a shell.
  pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let child_events = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;

    command
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true);
    configure_process(command);
    let child = command.spawn()?;

    #[cfg(unix)]
    {
      let process_group = child
        .id()
        .and_then(|id| i32::try_from(id).ok())
        .ok_or_else(|| io::Error::other("Codex process has no valid process id"))?;
      Ok(Self {
        child,
        process_group: Some(process_group),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        child_events,
      })
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

  /// Takes the only writable handle to the child's standard input.
  pub(crate) fn take_stdin(&mut self) -> io::Result<ChildStdin> {
    self
      .child
      .stdin
      .take()
      .ok_or_else(|| io::Error::other("Codex process did not expose stdin"))
  }

  /// Takes the only readable handle to the child's standard output.
  pub(crate) fn take_stdout(&mut self) -> io::Result<ChildStdout> {
    self
      .child
      .stdout
      .take()
      .ok_or_else(|| io::Error::other("Codex process did not expose stdout"))
  }

  /// Takes the only readable handle to the child's standard error.
  pub(crate) fn take_stderr(&mut self) -> io::Result<ChildStderr> {
    self
      .child
      .stderr
      .take()
      .ok_or_else(|| io::Error::other("Codex process did not expose stderr"))
  }

  /// Waits for the leader and terminates descendants before returning.
  pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
      let Some(pid) = self.process_group else {
        return self.child.wait().await;
      };
      loop {
        if process_exited_without_reaping(pid)? {
          // Disarm ownership before signalling. Once the direct child is
          // reaped the numeric PGID may be reused, so Drop must never signal
          // it a second time.
          let termination = self.close_exited_process_group();
          let status = self.child.wait().await;
          termination?;
          return status;
        }
        self.child_events.recv().await;
      }
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    {
      let status = self.child.wait().await;
      let termination = self.close_process_group(libc::SIGKILL);
      termination?;
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

  /// Requests cooperative termination without releasing process ownership.
  ///
  /// The coordinator closes stdin before calling this method. Unix additionally
  /// sends `SIGTERM` to the complete process group. Windows has no reliable
  /// console-control channel for a hidden process, so closed stdin is its
  /// portable graceful signal; the Job Object remains armed for force-kill.
  #[cfg(unix)]
  pub(crate) fn request_graceful_termination(&self) -> io::Result<()> {
    self.process_group.map_or(Ok(()), |process_group| {
      signal_process_group(process_group, libc::SIGTERM)
    })
  }

  /// Requests cooperative termination on platforms without Unix signals.
  #[cfg(not(unix))]
  pub(crate) fn request_graceful_termination(&self) -> io::Result<()> {
    Ok(())
  }

  /// Force-terminates the complete owned tree and reaps the direct child.
  pub(crate) async fn terminate(&mut self) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    let termination = match self.process_group {
      Some(process_group) if process_exited_without_reaping(process_group)? => self.close_exited_process_group(),
      _ => self.close_process_group(libc::SIGKILL),
    };

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    let termination = self.close_process_group(libc::SIGKILL);

    #[cfg(unix)]
    if termination.is_err() {
      // Preserve the group error, but still make a best effort to ensure the
      // direct child can be reaped instead of waiting forever.
      let _ = self.child.start_kill();
    }

    #[cfg(windows)]
    {
      self.job.take();
    }

    #[cfg(not(any(unix, windows)))]
    {
      let _ = self.child.start_kill();
    }

    let reaped = self.child.wait().await.map(|_| ());
    #[cfg(unix)]
    termination?;
    reaped
  }

  #[cfg(unix)]
  fn close_process_group(&mut self, signal: i32) -> io::Result<()> {
    self
      .process_group
      .take()
      .map_or(Ok(()), |process_group| signal_process_group(process_group, signal))
  }

  #[cfg(any(target_os = "linux", target_os = "macos"))]
  fn close_exited_process_group(&mut self) -> io::Result<()> {
    let Some(process_group) = self.process_group.take() else {
      return Ok(());
    };
    let result = signal_process_group(process_group, libc::SIGKILL);
    #[cfg(target_os = "macos")]
    if result
      .as_ref()
      .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
    {
      // macOS reports EPERM when the group contains only its waitable zombie
      // leader. `waitid(WNOWAIT)` above proves that leader has exited; a live
      // same-user descendant would make the group signal succeed.
      return Ok(());
    }
    result
  }
}

impl Drop for ProcessTree {
  fn drop(&mut self) {
    #[cfg(unix)]
    if let Some(process_group) = self.process_group.take() {
      let _ = signal_process_group(process_group, libc::SIGKILL);
    }

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
fn signal_process_group(process_group: i32, signal: i32) -> io::Result<()> {
  // SAFETY: the negative pid identifies the dedicated group created by
  // `configure_process`; no unrelated process can join it through this API.
  let result = unsafe { libc::kill(-process_group, signal) };
  if result == 0 {
    return Ok(());
  }
  let error = io::Error::last_os_error();
  if error.raw_os_error() == Some(libc::ESRCH) {
    return Ok(());
  }
  Err(error)
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
    .ok_or_else(|| io::Error::other("Codex process exited before Job Object assignment"))?;
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
    .ok_or_else(|| io::Error::other("suspended Codex process has no process id"))?;
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
    "suspended Codex process has no discoverable primary thread",
  ))
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
