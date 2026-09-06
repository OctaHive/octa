use std::{
  collections::VecDeque,
  fmt, io,
  sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex as StdMutex,
  },
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};

use tokio::sync::{mpsc, oneshot, Mutex, OwnedMutexGuard};

use super::{
  CliDocument, ConsoleDiagnostic, ConsoleEntry, ConsoleLevel, ConsolePayload, ConsoleRecord, ConsoleRenderer,
  ConsoleScope, ConsoleStream, ExecutionEvent, NullRenderer,
};

static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(0);
const WRITER_QUEUE_CAPACITY: usize = 256;
const RAW_DIAGNOSTIC_CAPACITY: usize = 256;

enum WriterRequest {
  Render {
    entry: ConsoleEntry,
    completed: Option<oneshot::Sender<io::Result<()>>>,
  },
  Barrier(oneshot::Sender<()>),
  SetParallel {
    parallel: bool,
    completed: oneshot::Sender<io::Result<()>>,
  },
  UpdateProgress {
    scope: ConsoleScope,
    update: ProgressUpdate,
    completed: oneshot::Sender<io::Result<()>>,
  },
  BeginRaw {
    scope: ConsoleScope,
    completed: oneshot::Sender<io::Result<()>>,
  },
  EndRaw {
    scope: ConsoleScope,
    entries: Vec<ConsoleEntry>,
  },
}

enum ProgressUpdate {
  Line(String),
  Bytes {
    command_id: String,
    stream: ConsoleStream,
    bytes: Vec<u8>,
  },
}

#[derive(Default)]
struct RawState {
  // Synchronous diagnostic producers cannot await `render_lock`, so records
  // arriving during a raw session are retained until that session is dropped.
  active: bool,
  pending: VecDeque<ConsoleEntry>,
}

/// Restores normal diagnostic routing if raw-session setup is cancelled.
struct RawActivation<'a> {
  console: &'a Console,
  scope: ConsoleScope,
  committed: bool,
}

impl<'a> RawActivation<'a> {
  fn new(console: &'a Console, scope: ConsoleScope) -> Self {
    console.raw_state.lock().unwrap().active = true;
    console.raw_ticks_paused.store(true, Ordering::Release);
    Self {
      console,
      scope,
      committed: false,
    }
  }

  fn commit(mut self) {
    self.committed = true;
  }
}

impl Drop for RawActivation<'_> {
  fn drop(&mut self) {
    if !self.committed {
      self.console.release_raw_state(self.scope.clone());
    }
  }
}

struct WriterHandle {
  sender: Option<mpsc::Sender<WriterRequest>>,
  thread: StdMutex<Option<JoinHandle<()>>>,
  background_error: Arc<StdMutex<Option<io::Error>>>,
  waker: thread::Thread,
}

impl WriterHandle {
  async fn send(&self, request: WriterRequest) -> io::Result<()> {
    self
      .sender
      .as_ref()
      .ok_or_else(writer_closed)?
      .send(request)
      .await
      .map_err(|_| writer_closed())?;
    self.waker.unpark();
    Ok(())
  }

  fn try_send(&self, request: WriterRequest) -> io::Result<()> {
    self
      .sender
      .as_ref()
      .ok_or_else(writer_closed)?
      .try_send(request)
      .map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => io::Error::new(io::ErrorKind::WouldBlock, "output writer queue is full"),
        mpsc::error::TrySendError::Closed(_) => writer_closed(),
      })?;
    self.waker.unpark();
    Ok(())
  }

  fn take_background_error(&self) -> Option<io::Error> {
    self.background_error.lock().unwrap().take()
  }
}

impl Drop for WriterHandle {
  fn drop(&mut self) {
    // Closing the final sender lets the worker drain accepted records before it exits.
    drop(self.sender.take());
    self.waker.unpark();
    if let Some(thread) = self.thread.lock().unwrap().take() {
      if thread.thread().id() != thread::current().id() {
        let _ = thread.join();
      }
    }
  }
}

/// Orders output from concurrent tasks and delegates blocking presentation to one writer thread.
pub struct Console {
  writer: WriterHandle,
  render_lock: Arc<Mutex<()>>,
  raw_state: StdMutex<RawState>,
  raw_ticks_paused: Arc<AtomicBool>,
  dropped_diagnostics: AtomicU64,
  raw_terminal_supported: bool,
  progress_updates_supported: bool,
}

impl Console {
  pub fn new(renderer: impl ConsoleRenderer) -> Self {
    let raw_terminal_supported = renderer.supports_raw_terminal();
    let progress_updates_supported = renderer.supports_progress_updates();
    let (sender, receiver) = mpsc::channel(WRITER_QUEUE_CAPACITY);
    let background_error = Arc::new(StdMutex::new(None));
    let raw_ticks_paused = Arc::new(AtomicBool::new(false));
    let writer = thread::Builder::new()
      .name("octa-output".to_owned())
      .spawn({
        let background_error = background_error.clone();
        let raw_ticks_paused = raw_ticks_paused.clone();
        move || render_loop(renderer, receiver, background_error, raw_ticks_paused)
      })
      .expect("failed to start output writer thread");
    let waker = writer.thread().clone();
    Self {
      writer: WriterHandle {
        sender: Some(sender),
        thread: StdMutex::new(Some(writer)),
        background_error,
        waker,
      },
      render_lock: Arc::new(Mutex::new(())),
      raw_state: StdMutex::new(RawState::default()),
      raw_ticks_paused,
      dropped_diagnostics: AtomicU64::new(0),
      raw_terminal_supported,
      progress_updates_supported,
    }
  }

  /// Allocates an identifier unique to this process for one execution run.
  pub fn allocate_run_id(&self) -> u64 {
    NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed)
  }

  /// Publishes a runtime event produced by the execution engine.
  pub async fn event(&self, event: ExecutionEvent) -> io::Result<()> {
    self.emit(ConsoleRecord::Execution(event)).await
  }

  /// Publishes a complete response produced by the CLI layer.
  pub async fn document(&self, document: CliDocument) -> io::Result<()> {
    self.emit(ConsoleRecord::Document(document)).await
  }

  pub async fn message(&self, level: ConsoleLevel, message: impl Into<String>) -> io::Result<()> {
    self.emit_message(None, None, level, message).await
  }

  /// Publishes a diagnostic associated with an execution run.
  pub async fn run_message(&self, run_id: u64, level: ConsoleLevel, message: impl Into<String>) -> io::Result<()> {
    self.emit_message(Some(run_id), None, level, message).await
  }

  pub async fn run_diagnostic(
    &self,
    run_id: u64,
    level: ConsoleLevel,
    message: impl Into<String>,
    location: Option<super::SourceLocation>,
  ) -> io::Result<()> {
    self
      .emit_diagnostic(Some(run_id), None, None, level, message, location)
      .await
  }

  /// Publishes a diagnostic associated with a scope inside an execution run.
  pub async fn run_message_at(
    &self,
    run_id: u64,
    scope: ConsoleScope,
    level: ConsoleLevel,
    message: impl Into<String>,
  ) -> io::Result<()> {
    self.emit_message(Some(run_id), Some(scope), level, message).await
  }

  pub async fn run_diagnostic_at(
    &self,
    run_id: u64,
    scope: ConsoleScope,
    level: ConsoleLevel,
    message: impl Into<String>,
    location: Option<super::SourceLocation>,
  ) -> io::Result<()> {
    self
      .emit_diagnostic(Some(run_id), Some(scope), None, level, message, location)
      .await
  }

  /// Queues a diagnostic from synchronous integrations such as a tracing layer.
  ///
  /// The bounded queue returns `WouldBlock` rather than delaying a synchronous
  /// producer. A background renderer failure is reported by the next awaited write.
  pub fn message_nowait(&self, level: ConsoleLevel, message: impl Into<String>) -> io::Result<()> {
    self.emit_message_nowait(None, level, message)
  }

  /// Queues a synchronous diagnostic associated with an execution run.
  pub fn run_message_nowait(&self, run_id: u64, level: ConsoleLevel, message: impl Into<String>) -> io::Result<()> {
    self.emit_message_nowait(Some(run_id), level, message)
  }

  fn emit_message_nowait(
    &self,
    run_id: Option<u64>,
    level: ConsoleLevel,
    message: impl Into<String>,
  ) -> io::Result<()> {
    let entry = ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id,
      scope: None,
      step_id: None,
      level,
      message: message.into(),
      location: None,
    }));
    let mut state = self.raw_state.lock().unwrap();
    if state.active {
      if state.pending.len() == RAW_DIAGNOSTIC_CAPACITY {
        self.dropped_diagnostics.fetch_add(1, Ordering::Relaxed);
        return Err(io::Error::new(
          io::ErrorKind::WouldBlock,
          "raw diagnostic buffer is full",
        ));
      }
      state.pending.push_back(entry);
      return Ok(());
    }

    let result = self.try_send(WriterRequest::Render { entry, completed: None });
    if result
      .as_ref()
      .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock)
    {
      self.dropped_diagnostics.fetch_add(1, Ordering::Relaxed);
    }
    result
  }

  /// Reserves the output stream until the returned raw session is dropped.
  pub async fn begin_raw(
    self: &Arc<Self>,
    run_id: u64,
    scope: ConsoleScope,
    command_id: impl Into<String>,
  ) -> io::Result<RawConsoleSession> {
    self.begin_raw_for_step(run_id, scope, None, command_id).await
  }

  /// Reserves output for an interactive command associated with a stable execution step.
  pub async fn begin_raw_for_step(
    self: &Arc<Self>,
    run_id: u64,
    scope: ConsoleScope,
    step_id: Option<u64>,
    command_id: impl Into<String>,
  ) -> io::Result<RawConsoleSession> {
    if !self.raw_terminal_supported {
      return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "raw/PTY mode is incompatible with the selected output renderer",
      ));
    }
    let command_id = command_id.into();
    let guard = self.render_lock.clone().lock_owned().await;
    let activation = RawActivation::new(self, scope.clone());
    let (completed, result) = oneshot::channel();
    self
      .writer
      .send(WriterRequest::BeginRaw {
        scope: scope.clone(),
        completed,
      })
      .await?;
    let begin_result = result.await.map_err(|_| writer_closed())?;
    if let Some(error) = self.writer.take_background_error() {
      if let Err(current) = begin_result {
        store_background_error(&self.writer.background_error, current);
      }
      return Err(error);
    }
    begin_result?;
    activation.commit();

    Ok(RawConsoleSession {
      console: self.clone(),
      run_id,
      scope,
      step_id,
      command_id,
      _guard: guard,
    })
  }

  async fn emit_message(
    &self,
    run_id: Option<u64>,
    scope: Option<ConsoleScope>,
    level: ConsoleLevel,
    message: impl Into<String>,
  ) -> io::Result<()> {
    self.emit_diagnostic(run_id, scope, None, level, message, None).await
  }

  /// Publishes a diagnostic associated with one executable step.
  pub async fn run_diagnostic_at_step(
    &self,
    run_id: u64,
    scope: ConsoleScope,
    step_id: u64,
    level: ConsoleLevel,
    message: impl Into<String>,
    location: Option<super::SourceLocation>,
  ) -> io::Result<()> {
    self
      .emit_diagnostic(Some(run_id), Some(scope), Some(step_id), level, message, location)
      .await
  }

  async fn emit_diagnostic(
    &self,
    run_id: Option<u64>,
    scope: Option<ConsoleScope>,
    step_id: Option<u64>,
    level: ConsoleLevel,
    message: impl Into<String>,
    location: Option<super::SourceLocation>,
  ) -> io::Result<()> {
    self
      .emit(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        run_id,
        scope,
        step_id,
        level,
        message: message.into(),
        location,
      }))
      .await
  }

  async fn emit(&self, record: ConsoleRecord) -> io::Result<()> {
    let entry = ConsoleEntry::new(record);
    // Raw sessions hold this lock across writes; regular records hold it only until
    // the writer acknowledges the record, preserving a single global output order.
    let _guard = self.render_lock.lock().await;
    self.report_dropped_diagnostics().await?;
    self.render(entry).await
  }

  async fn report_dropped_diagnostics(&self) -> io::Result<()> {
    let dropped = self.dropped_diagnostics.swap(0, Ordering::Relaxed);
    if dropped == 0 {
      return Ok(());
    }

    let result = self
      .render(ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        run_id: None,
        scope: None,
        step_id: None,
        level: ConsoleLevel::Warn,
        message: format!("Dropped {dropped} diagnostics because the output buffer was full"),
        location: None,
      })))
      .await;
    if result.is_err() {
      self.dropped_diagnostics.fetch_add(dropped, Ordering::Relaxed);
    }
    result
  }

  async fn render(&self, entry: ConsoleEntry) -> io::Result<()> {
    let (completed, result) = oneshot::channel();
    self
      .writer
      .send(WriterRequest::Render {
        entry,
        completed: Some(completed),
      })
      .await?;
    let render_result = result.await.map_err(|_| writer_closed())?;
    match self.writer.take_background_error() {
      Some(error) => {
        if let Err(current) = render_result {
          store_background_error(&self.writer.background_error, current);
        }
        Err(error)
      },
      None => render_result,
    }
  }

  /// Waits until all records accepted before this call have been rendered.
  pub async fn drain(&self) -> io::Result<()> {
    let _guard = self.render_lock.lock().await;
    self.report_dropped_diagnostics().await?;
    self.writer_barrier().await
  }

  /// Tells adaptive presentation whether the built execution plan can run work concurrently.
  pub async fn set_parallel(&self, parallel: bool) -> io::Result<()> {
    let _guard = self.render_lock.lock().await;
    let (completed, result) = oneshot::channel();
    self
      .writer
      .send(WriterRequest::SetParallel { parallel, completed })
      .await?;
    let render_result = result.await.map_err(|_| writer_closed())?;
    match self.writer.take_background_error() {
      Some(error) => {
        if let Err(current) = render_result {
          store_background_error(&self.writer.background_error, current);
        }
        Err(error)
      },
      None => render_result,
    }
  }

  /// Sends a presentation-only progress update in the same order as runtime records.
  pub async fn update_progress(&self, scope: ConsoleScope, message: impl Into<String>) -> io::Result<()> {
    self.send_progress(scope, ProgressUpdate::Line(message.into())).await
  }

  /// Sends a byte-oriented progress update without waiting for a line ending.
  pub async fn update_progress_bytes(
    &self,
    scope: ConsoleScope,
    command_id: impl Into<String>,
    stream: ConsoleStream,
    bytes: Vec<u8>,
  ) -> io::Result<()> {
    self
      .send_progress(
        scope,
        ProgressUpdate::Bytes {
          command_id: command_id.into(),
          stream,
          bytes,
        },
      )
      .await
  }

  async fn send_progress(&self, scope: ConsoleScope, update: ProgressUpdate) -> io::Result<()> {
    if !self.progress_updates_supported {
      return Ok(());
    }
    let _guard = self.render_lock.lock().await;
    let (completed, result) = oneshot::channel();
    self
      .writer
      .send(WriterRequest::UpdateProgress {
        scope,
        update,
        completed,
      })
      .await?;
    let render_result = result.await.map_err(|_| writer_closed())?;
    match self.writer.take_background_error() {
      Some(error) => {
        if let Err(current) = render_result {
          store_background_error(&self.writer.background_error, current);
        }
        Err(error)
      },
      None => render_result,
    }
  }

  async fn writer_barrier(&self) -> io::Result<()> {
    let (completed, result) = oneshot::channel();
    self.writer.send(WriterRequest::Barrier(completed)).await?;
    result.await.map_err(|_| writer_closed())?;
    self.writer.take_background_error().map_or(Ok(()), Err)
  }

  fn try_send(&self, request: WriterRequest) -> io::Result<()> {
    self.writer.try_send(request)
  }

  fn release_raw_state(&self, scope: ConsoleScope) {
    let mut state = self.raw_state.lock().unwrap();
    let pending = state.pending.drain(..).collect::<Vec<_>>();
    let count = pending.len() as u64;
    if self
      .try_send(WriterRequest::EndRaw {
        scope,
        entries: pending,
      })
      .is_err()
    {
      self.dropped_diagnostics.fetch_add(count, Ordering::Relaxed);
    }
    // Keep the state lock until the pending batch has been queued so a new
    // synchronous diagnostic cannot overtake records buffered by the session.
    state.active = false;
    self.raw_ticks_paused.store(false, Ordering::Release);
    self.writer.waker.unpark();
  }
}

/// The default sink is intentionally silent; applications select presentation explicitly.
impl Default for Console {
  fn default() -> Self {
    Self::new(NullRenderer)
  }
}

impl fmt::Debug for Console {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("Console").finish_non_exhaustive()
  }
}

/// Exclusive byte-oriented output session used by future PTY transports.
pub struct RawConsoleSession {
  console: Arc<Console>,
  run_id: u64,
  scope: ConsoleScope,
  step_id: Option<u64>,
  command_id: String,
  _guard: OwnedMutexGuard<()>,
}

impl RawConsoleSession {
  pub async fn write(&mut self, stream: ConsoleStream, bytes: impl Into<Vec<u8>>) -> io::Result<()> {
    self
      .console
      .render(ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: self.run_id,
        scope: Some(self.scope.clone()),
        step_id: self.step_id,
        command_id: self.command_id.clone(),
        stream,
        payload: ConsolePayload::RawBytes(bytes.into()),
      })))
      .await
  }
}

impl Drop for RawConsoleSession {
  fn drop(&mut self) {
    self.console.release_raw_state(self.scope.clone());
  }
}

fn render_loop(
  mut renderer: impl ConsoleRenderer,
  mut receiver: mpsc::Receiver<WriterRequest>,
  background_error: Arc<StdMutex<Option<io::Error>>>,
  raw_ticks_paused: Arc<AtomicBool>,
) {
  const TICK_INTERVAL: Duration = Duration::from_millis(100);
  let mut next_sequence = 1;
  let mut last_tick = Instant::now();
  loop {
    if raw_ticks_paused.load(Ordering::Acquire) {
      last_tick = Instant::now();
    } else if last_tick.elapsed() >= TICK_INTERVAL {
      if let Err(error) = renderer.tick() {
        store_background_error(&background_error, error);
      }
      last_tick = Instant::now();
    }
    let request = match receiver.try_recv() {
      Ok(request) => request,
      Err(mpsc::error::TryRecvError::Empty) => {
        if receiver.is_closed() {
          break;
        }
        thread::park_timeout(TICK_INTERVAL.saturating_sub(last_tick.elapsed()));
        continue;
      },
      Err(mpsc::error::TryRecvError::Disconnected) => break,
    };
    match request {
      WriterRequest::Render { mut entry, completed } => {
        entry.assign_sequence(next_sequence);
        next_sequence += 1;
        let result = renderer.render(&entry);
        if let Some(completed) = completed {
          if let Err(Err(error)) = completed.send(result) {
            store_background_error(&background_error, error);
          }
        } else if let Err(error) = result {
          store_background_error(&background_error, error);
        }
      },
      WriterRequest::SetParallel { parallel, completed } => {
        let result = renderer.set_parallel(parallel);
        if let Err(Err(error)) = completed.send(result) {
          store_background_error(&background_error, error);
        }
      },
      WriterRequest::UpdateProgress {
        scope,
        update,
        completed,
      } => {
        let result = match update {
          ProgressUpdate::Line(message) => renderer.update_progress(&scope, &message),
          ProgressUpdate::Bytes {
            command_id,
            stream,
            bytes,
          } => renderer.update_progress_bytes(&scope, &command_id, stream, &bytes),
        };
        if let Err(Err(error)) = completed.send(result) {
          store_background_error(&background_error, error);
        }
      },
      WriterRequest::Barrier(completed) => {
        let _ = completed.send(());
      },
      WriterRequest::BeginRaw { scope, completed } => {
        let _ = completed.send(renderer.begin_raw(&scope));
      },
      WriterRequest::EndRaw { scope, entries } => {
        for mut entry in entries {
          entry.assign_sequence(next_sequence);
          next_sequence += 1;
          if let Err(error) = renderer.render(&entry) {
            store_background_error(&background_error, error);
          }
        }
        if let Err(error) = renderer.end_raw(&scope) {
          store_background_error(&background_error, error);
        }
      },
    }
  }
}

fn store_background_error(slot: &StdMutex<Option<io::Error>>, error: io::Error) {
  let mut slot = slot.lock().unwrap();
  if slot.is_none() {
    *slot = Some(error);
  }
}

fn writer_closed() -> io::Error {
  io::Error::new(io::ErrorKind::BrokenPipe, "output writer thread stopped")
}

#[cfg(test)]
mod tests {
  use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc as std_mpsc, Mutex as StdMutex,
  };
  use std::time::Duration;

  use super::*;
  use crate::{ConsoleRecord, ConsoleRenderer, ConsoleScopeAllocator};

  type RecordedProgressBytes = (ConsoleScope, String, ConsoleStream, Vec<u8>);

  #[derive(Default)]
  struct RecordingRenderer {
    records: StdMutex<Vec<ConsoleRecord>>,
    sequences: StdMutex<Vec<u64>>,
    threads: StdMutex<Vec<thread::ThreadId>>,
    progress: StdMutex<Vec<(ConsoleScope, String)>>,
    progress_bytes: StdMutex<Vec<RecordedProgressBytes>>,
  }

  impl ConsoleRenderer for Arc<RecordingRenderer> {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.records.lock().unwrap().push(entry.record().clone());
      self.sequences.lock().unwrap().push(entry.sequence());
      self.threads.lock().unwrap().push(thread::current().id());
      Ok(())
    }

    fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
      self.progress.lock().unwrap().push((scope.clone(), message.to_owned()));
      Ok(())
    }

    fn update_progress_bytes(
      &mut self,
      scope: &ConsoleScope,
      command_id: &str,
      stream: ConsoleStream,
      bytes: &[u8],
    ) -> io::Result<()> {
      self
        .progress_bytes
        .lock()
        .unwrap()
        .push((scope.clone(), command_id.to_owned(), stream, bytes.to_vec()));
      Ok(())
    }

    fn supports_progress_updates(&self) -> bool {
      true
    }
  }

  fn recording_console() -> (Arc<Console>, Arc<RecordingRenderer>) {
    let renderer = Arc::new(RecordingRenderer::default());
    let console = Arc::new(Console::new(renderer.clone()));
    (console, renderer)
  }

  #[tokio::test]
  async fn renderer_runs_on_the_writer_thread() {
    let caller = thread::current().id();
    let (console, renderer) = recording_console();

    console.message(ConsoleLevel::Info, "message").await.unwrap();

    assert_ne!(renderer.threads.lock().unwrap()[0], caller);
    assert_eq!(format!("{console:?}"), "Console { .. }");
  }

  #[test]
  fn run_ids_are_unique_across_consoles() {
    let (first, _) = recording_console();
    let (second, _) = recording_console();

    assert_ne!(first.allocate_run_id(), second.allocate_run_id());
  }

  #[tokio::test]
  async fn events_preserve_contents_and_metadata() {
    let (console, renderer) = recording_console();
    let scope = ConsoleScopeAllocator::default().scope("build");

    console
      .event(ExecutionEvent::Output {
        run_id: 7,
        scope: Some(scope.clone()),
        step_id: None,
        command_id: "command-1".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("  value  ".to_owned()),
      })
      .await
      .unwrap();

    assert_eq!(
      *renderer.records.lock().unwrap(),
      vec![ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 7,
        scope: Some(scope),
        step_id: None,
        command_id: "command-1".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("  value  ".to_owned()),
      })]
    );
  }

  #[tokio::test]
  async fn events_support_unscoped_output() {
    let (console, renderer) = recording_console();

    console
      .event(ExecutionEvent::Output {
        run_id: 7,
        scope: None,
        step_id: None,
        command_id: "command-1".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line("value".to_owned()),
      })
      .await
      .unwrap();

    assert!(matches!(
      &renderer.records.lock().unwrap()[0],
      ConsoleRecord::Execution(ExecutionEvent::Output { scope: None, .. })
    ));
  }

  #[tokio::test]
  async fn progress_updates_use_the_writer_without_becoming_records() {
    let (console, renderer) = recording_console();
    let scope = ConsoleScopeAllocator::default().scope("build");

    console.update_progress(scope.clone(), "compiling").await.unwrap();
    console
      .update_progress_bytes(scope.clone(), "command", ConsoleStream::Stderr, b"partial".to_vec())
      .await
      .unwrap();

    assert_eq!(*renderer.progress.lock().unwrap(), [(scope, "compiling".to_owned())]);
    assert!(matches!(
      renderer.progress_bytes.lock().unwrap().as_slice(),
      [(_, command_id, ConsoleStream::Stderr, bytes)]
        if command_id == "command" && bytes == b"partial"
    ));
    assert!(renderer.records.lock().unwrap().is_empty());
  }

  #[tokio::test]
  async fn assigns_monotonic_sequence_numbers_on_the_writer() {
    let (console, renderer) = recording_console();
    console.message(ConsoleLevel::Info, "one").await.unwrap();
    console.message(ConsoleLevel::Info, "two").await.unwrap();
    assert_eq!(*renderer.sequences.lock().unwrap(), [1, 2]);
  }

  struct NoRawRenderer;

  impl ConsoleRenderer for NoRawRenderer {
    fn render(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
      Ok(())
    }

    fn supports_raw_terminal(&self) -> bool {
      false
    }
  }

  #[tokio::test]
  async fn rejects_raw_sessions_for_incompatible_renderers() {
    let console = Arc::new(Console::new(NoRawRenderer));
    let error = console
      .begin_raw(1, ConsoleScopeAllocator::default().scope("raw"), "command")
      .await
      .err()
      .expect("raw mode must be rejected");
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
  }

  #[tokio::test]
  async fn cli_documents_use_the_same_writer() {
    let (console, renderer) = recording_console();

    console
      .document(CliDocument::Completion {
        text: "complete".to_owned(),
      })
      .await
      .unwrap();

    assert_eq!(
      renderer.records.lock().unwrap()[0],
      ConsoleRecord::Document(CliDocument::Completion {
        text: "complete".to_owned(),
      })
    );
  }

  #[tokio::test]
  async fn raw_session_excludes_regular_events() {
    let (console, renderer) = recording_console();
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut session = console.begin_raw(7, scope.clone(), "command-1").await.unwrap();
    let regular_finished = Arc::new(AtomicBool::new(false));
    let handle = tokio::spawn({
      let console = console.clone();
      let regular_finished = regular_finished.clone();
      async move {
        console.message(ConsoleLevel::Info, "after raw").await.unwrap();
        regular_finished.store(true, Ordering::SeqCst);
      }
    });

    session.write(ConsoleStream::Stdout, b"raw".to_vec()).await.unwrap();
    tokio::task::yield_now().await;
    assert!(!regular_finished.load(Ordering::SeqCst));
    drop(session);
    handle.await.unwrap();

    assert_eq!(
      renderer.records.lock().unwrap()[0],
      ConsoleRecord::Execution(ExecutionEvent::Output {
        run_id: 7,
        scope: Some(scope),
        step_id: None,
        command_id: "command-1".to_owned(),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::RawBytes(b"raw".to_vec()),
      })
    );
  }

  struct RawLifecycleRenderer(Arc<StdMutex<Vec<&'static str>>>);

  impl ConsoleRenderer for RawLifecycleRenderer {
    fn render(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
      Ok(())
    }

    fn begin_raw(&mut self, _scope: &ConsoleScope) -> io::Result<()> {
      self.0.lock().unwrap().push("begin");
      Ok(())
    }

    fn end_raw(&mut self, _scope: &ConsoleScope) -> io::Result<()> {
      self.0.lock().unwrap().push("end");
      Ok(())
    }
  }

  #[tokio::test]
  async fn raw_lifecycle_is_reported_even_when_the_process_writes_nothing() {
    let lifecycle = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RawLifecycleRenderer(lifecycle.clone())));
    let session = console
      .begin_raw(1, ConsoleScopeAllocator::default().scope("prompt"), "command")
      .await
      .unwrap();
    drop(session);
    console.drain().await.unwrap();
    assert_eq!(*lifecycle.lock().unwrap(), ["begin", "end"]);
  }

  #[tokio::test]
  async fn synchronous_diagnostics_wait_until_raw_output_finishes() {
    let (console, renderer) = recording_console();
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let mut session = console.begin_raw(7, scope, "command-1").await.unwrap();

    console.message_nowait(ConsoleLevel::Debug, "buffered").unwrap();
    session.write(ConsoleStream::Stdout, b"raw".to_vec()).await.unwrap();
    drop(session);
    console.message(ConsoleLevel::Info, "barrier").await.unwrap();

    let records = renderer.records.lock().unwrap();
    assert!(matches!(
      &records[0],
      ConsoleRecord::Execution(ExecutionEvent::Output {
        payload: ConsolePayload::RawBytes(bytes),
        ..
      }) if bytes == b"raw"
    ));
    assert!(matches!(
      &records[1],
      ConsoleRecord::Diagnostic(diagnostic) if diagnostic.message == "buffered"
    ));
  }

  struct DropRenderer(Arc<AtomicBool>);

  impl ConsoleRenderer for DropRenderer {
    fn render(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
      Ok(())
    }
  }

  impl Drop for DropRenderer {
    fn drop(&mut self) {
      self.0.store(true, Ordering::SeqCst);
    }
  }

  #[test]
  fn dropping_console_joins_and_releases_the_writer() {
    let dropped = Arc::new(AtomicBool::new(false));
    let console = Console::new(DropRenderer(dropped.clone()));

    drop(console);

    assert!(dropped.load(Ordering::SeqCst));
  }

  struct TickRenderer(std_mpsc::Sender<()>);

  impl ConsoleRenderer for TickRenderer {
    fn render(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
      Ok(())
    }

    fn tick(&mut self) -> io::Result<()> {
      let _ = self.0.send(());
      Ok(())
    }
  }

  #[test]
  fn writer_advances_time_based_renderers_without_new_events() {
    let (sender, receiver) = std_mpsc::channel();
    let console = Console::new(TickRenderer(sender));
    receiver
      .recv_timeout(Duration::from_secs(1))
      .expect("writer did not tick renderer");
    drop(console);
  }

  struct FailingRenderer;

  impl ConsoleRenderer for FailingRenderer {
    fn render(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
      Err(io::Error::other("renderer failed"))
    }
  }

  #[tokio::test]
  async fn renderer_errors_are_returned_to_the_caller() {
    let console = Console::new(FailingRenderer);

    let error = console.message(ConsoleLevel::Info, "message").await.unwrap_err();

    assert_eq!(error.kind(), io::ErrorKind::Other);
  }

  struct FailsOnceRenderer(AtomicBool);

  impl ConsoleRenderer for FailsOnceRenderer {
    fn render(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
      if !self.0.swap(true, Ordering::SeqCst) {
        Err(io::Error::other("background render failed"))
      } else {
        Ok(())
      }
    }
  }

  #[tokio::test]
  async fn awaited_writes_report_background_renderer_errors() {
    let console = Console::new(FailsOnceRenderer(AtomicBool::new(false)));

    console.message_nowait(ConsoleLevel::Debug, "background").unwrap();
    let error = console.message(ConsoleLevel::Info, "barrier").await.unwrap_err();

    assert_eq!(error.to_string(), "background render failed");
    console.message(ConsoleLevel::Info, "recovered").await.unwrap();
  }

  struct FailsTwiceRenderer(AtomicUsize);

  impl ConsoleRenderer for FailsTwiceRenderer {
    fn render(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
      match self.0.fetch_add(1, Ordering::SeqCst) {
        0 => Err(io::Error::other("first failure")),
        1 => Err(io::Error::other("second failure")),
        _ => Ok(()),
      }
    }
  }

  #[tokio::test]
  async fn preserves_an_awaited_error_hidden_by_an_earlier_background_error() {
    let console = Console::new(FailsTwiceRenderer(AtomicUsize::new(0)));

    console.message_nowait(ConsoleLevel::Debug, "background").unwrap();
    let first = console.message(ConsoleLevel::Info, "awaited").await.unwrap_err();
    let second = console.message(ConsoleLevel::Info, "next").await.unwrap_err();

    assert_eq!(first.to_string(), "first failure");
    assert_eq!(second.to_string(), "second failure");
    console.message(ConsoleLevel::Info, "recovered").await.unwrap();
  }

  #[tokio::test]
  async fn drain_waits_for_synchronous_diagnostics() {
    let (console, renderer) = recording_console();

    console.message_nowait(ConsoleLevel::Debug, "background").unwrap();
    console.drain().await.unwrap();

    assert!(matches!(
      &renderer.records.lock().unwrap()[0],
      ConsoleRecord::Diagnostic(diagnostic) if diagnostic.message == "background"
    ));
  }

  #[tokio::test]
  async fn drain_reports_background_renderer_errors() {
    let console = Console::new(FailsOnceRenderer(AtomicBool::new(false)));

    console.message_nowait(ConsoleLevel::Debug, "background").unwrap();
    let error = console.drain().await.unwrap_err();

    assert_eq!(error.to_string(), "background render failed");
  }

  #[tokio::test]
  async fn failed_raw_barrier_releases_the_session_lock() {
    let console = Arc::new(Console::new(FailsOnceRenderer(AtomicBool::new(false))));
    let allocator = ConsoleScopeAllocator::default();

    console.message_nowait(ConsoleLevel::Debug, "background").unwrap();
    let error = match console.begin_raw(7, allocator.scope("first"), "command-1").await {
      Ok(_) => panic!("raw session unexpectedly started"),
      Err(error) => error,
    };
    assert_eq!(error.to_string(), "background render failed");

    let session = console
      .begin_raw(7, allocator.scope("second"), "command-2")
      .await
      .unwrap();
    drop(session);
  }

  struct BlockingRenderer {
    entered: StdMutex<Option<oneshot::Sender<()>>>,
    release: StdMutex<std_mpsc::Receiver<()>>,
    records: Arc<StdMutex<Vec<ConsoleRecord>>>,
    fail: bool,
  }

  impl ConsoleRenderer for BlockingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.records.lock().unwrap().push(entry.record().clone());
      if let Some(entered) = self.entered.lock().unwrap().take() {
        entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
      }
      if self.fail {
        Err(io::Error::other("blocked renderer failed"))
      } else {
        Ok(())
      }
    }
  }

  #[tokio::test]
  async fn cancelling_raw_setup_restores_normal_diagnostic_routing() {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std_mpsc::channel();
    let records = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(BlockingRenderer {
      entered: StdMutex::new(Some(entered_tx)),
      release: StdMutex::new(release_rx),
      records: records.clone(),
      fail: false,
    }));
    let allocator = ConsoleScopeAllocator::default();

    console.message_nowait(ConsoleLevel::Debug, "blocking").unwrap();
    entered_rx.await.unwrap();
    let setup = tokio::spawn({
      let console = console.clone();
      async move { console.begin_raw(7, allocator.scope("raw"), "command-1").await }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
      loop {
        if console.raw_state.lock().unwrap().active {
          break;
        }
        tokio::task::yield_now().await;
      }
    })
    .await
    .unwrap();
    console.message_nowait(ConsoleLevel::Debug, "buffered").unwrap();

    setup.abort();
    assert!(matches!(setup.await, Err(error) if error.is_cancelled()));
    {
      let state = console.raw_state.lock().unwrap();
      assert!(!state.active);
      assert!(state.pending.is_empty());
    }

    console
      .message_nowait(ConsoleLevel::Debug, "after cancellation")
      .unwrap();
    release_tx.send(()).unwrap();
    console.drain().await.unwrap();

    let records = records.lock().unwrap();
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic) if diagnostic.message == "buffered"
    )));
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic) if diagnostic.message == "after cancellation"
    )));
  }

  #[tokio::test]
  async fn cancelled_awaited_write_preserves_its_renderer_error() {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std_mpsc::channel();
    let console = Arc::new(Console::new(BlockingRenderer {
      entered: StdMutex::new(Some(entered_tx)),
      release: StdMutex::new(release_rx),
      records: Arc::new(StdMutex::new(Vec::new())),
      fail: true,
    }));
    let write = tokio::spawn({
      let console = console.clone();
      async move { console.message(ConsoleLevel::Info, "cancelled").await }
    });

    entered_rx.await.unwrap();
    write.abort();
    assert!(write.await.unwrap_err().is_cancelled());
    release_tx.send(()).unwrap();

    let error = console.drain().await.unwrap_err();
    assert_eq!(error.to_string(), "blocked renderer failed");
  }

  struct FailsFirstRecordingRenderer {
    records: Arc<StdMutex<Vec<ConsoleRecord>>>,
    failed: bool,
  }

  impl ConsoleRenderer for FailsFirstRecordingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.records.lock().unwrap().push(entry.record().clone());
      if !self.failed {
        self.failed = true;
        Err(io::Error::other("first buffered record failed"))
      } else {
        Ok(())
      }
    }
  }

  #[tokio::test]
  async fn raw_batch_continues_after_a_renderer_error() {
    let records = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(FailsFirstRecordingRenderer {
      records: records.clone(),
      failed: false,
    }));
    let scope = ConsoleScopeAllocator::default().scope("raw");
    let session = console.begin_raw(7, scope, "command-1").await.unwrap();

    console.message_nowait(ConsoleLevel::Debug, "first").unwrap();
    console.message_nowait(ConsoleLevel::Debug, "second").unwrap();
    drop(session);

    let error = console.drain().await.unwrap_err();
    assert_eq!(error.to_string(), "first buffered record failed");
    let records = records.lock().unwrap();
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic) if diagnostic.message == "first"
    )));
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic) if diagnostic.message == "second"
    )));
  }

  #[tokio::test]
  async fn synchronous_diagnostics_apply_bounded_backpressure() {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = std_mpsc::channel();
    let console = Console::new(BlockingRenderer {
      entered: StdMutex::new(Some(entered_tx)),
      release: StdMutex::new(release_rx),
      records: Arc::new(StdMutex::new(Vec::new())),
      fail: false,
    });

    console.message_nowait(ConsoleLevel::Debug, "blocking").unwrap();
    entered_rx.await.unwrap();
    for index in 0..WRITER_QUEUE_CAPACITY {
      console
        .message_nowait(ConsoleLevel::Debug, format!("queued {index}"))
        .unwrap();
    }
    let error = console.message_nowait(ConsoleLevel::Debug, "overflow").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

    release_tx.send(()).unwrap();
    console.message(ConsoleLevel::Info, "barrier").await.unwrap();
  }

  #[tokio::test]
  async fn raw_diagnostic_buffer_is_bounded() {
    let (console, renderer) = recording_console();
    let scope = ConsoleScopeAllocator::default().scope("interactive");
    let session = console.begin_raw(7, scope, "command-1").await.unwrap();

    for index in 0..RAW_DIAGNOSTIC_CAPACITY {
      console
        .message_nowait(ConsoleLevel::Debug, format!("buffered {index}"))
        .unwrap();
    }
    let error = console.message_nowait(ConsoleLevel::Debug, "overflow").unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

    drop(session);
    console.message(ConsoleLevel::Info, "barrier").await.unwrap();

    assert!(renderer.records.lock().unwrap().iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic)
        if diagnostic.level == ConsoleLevel::Warn && diagnostic.message.contains("Dropped 1 diagnostics")
    )));
  }

  #[tokio::test]
  async fn message_helpers_attach_only_the_requested_context() {
    let (console, renderer) = recording_console();
    let scope = ConsoleScopeAllocator::default().scope("build");

    console.message(ConsoleLevel::Info, "global").await.unwrap();
    console.run_message(7, ConsoleLevel::Debug, "run").await.unwrap();
    console
      .run_message_nowait(7, ConsoleLevel::Warn, "synchronous run")
      .unwrap();
    console
      .run_message_at(7, scope.clone(), ConsoleLevel::Error, "run task")
      .await
      .unwrap();

    assert_eq!(
      *renderer.records.lock().unwrap(),
      vec![
        ConsoleRecord::Diagnostic(ConsoleDiagnostic {
          run_id: None,
          scope: None,
          step_id: None,
          level: ConsoleLevel::Info,
          message: "global".to_owned(),
          location: None,
        }),
        ConsoleRecord::Diagnostic(ConsoleDiagnostic {
          run_id: Some(7),
          scope: None,
          step_id: None,
          level: ConsoleLevel::Debug,
          message: "run".to_owned(),
          location: None,
        }),
        ConsoleRecord::Diagnostic(ConsoleDiagnostic {
          run_id: Some(7),
          scope: None,
          step_id: None,
          level: ConsoleLevel::Warn,
          message: "synchronous run".to_owned(),
          location: None,
        }),
        ConsoleRecord::Diagnostic(ConsoleDiagnostic {
          run_id: Some(7),
          scope: Some(scope),
          step_id: None,
          level: ConsoleLevel::Error,
          message: "run task".to_owned(),
          location: None,
        }),
      ]
    );
  }
}
