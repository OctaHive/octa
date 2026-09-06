use std::io;

use super::ConsoleEntry;

/// Receives fully sequenced console entries independently of their presentation.
///
/// The sink runs on the dedicated console writer thread and must not call back into
/// [`crate::Console`]. Implementations that perform asynchronous I/O should forward entries to a
/// bounded application-owned channel and define their own backpressure policy there.
pub trait EventSink: Send + 'static {
  fn emit(&mut self, entry: &ConsoleEntry) -> io::Result<()>;
}

impl<F> EventSink for F
where
  F: FnMut(&ConsoleEntry) -> io::Result<()> + Send + 'static,
{
  fn emit(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    self(entry)
  }
}

impl EventSink for Box<dyn EventSink> {
  fn emit(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    (**self).emit(entry)
  }
}

/// Event sink used when an application only needs rendered output.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullEventSink;

impl EventSink for NullEventSink {
  fn emit(&mut self, _entry: &ConsoleEntry) -> io::Result<()> {
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
  };

  use super::*;
  use crate::{ConsoleDiagnostic, ConsoleLevel, ConsoleRecord};

  fn entry() -> ConsoleEntry {
    ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
      run_id: None,
      scope: None,
      step_id: None,
      level: ConsoleLevel::Info,
      message: "event".to_owned(),
      location: None,
    }))
  }

  #[test]
  fn closures_and_boxed_sinks_are_supported() {
    let calls = Arc::new(AtomicUsize::new(0));
    let closure_calls = calls.clone();
    let mut closure = move |_entry: &ConsoleEntry| {
      closure_calls.fetch_add(1, Ordering::Relaxed);
      Ok(())
    };
    closure.emit(&entry()).unwrap();

    let mut boxed: Box<dyn EventSink> = Box::new(NullEventSink);
    boxed.emit(&entry()).unwrap();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
  }
}
