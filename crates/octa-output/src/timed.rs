use std::{
  collections::HashMap,
  io,
  time::{Duration, Instant},
};

use super::{
  ConsoleEntry, ConsolePayload, ConsoleRecord, ConsoleRenderer, ConsoleScope, ConsoleStream, ExecutionEvent,
};

const VISIBILITY_THRESHOLD: Duration = Duration::from_secs(1);
const MAX_PENDING_BYTES: usize = 64 * 1024;

enum PendingPayload {
  Line(ConsoleEntry),
  Bytes {
    reference: ConsoleEntry,
    buffered: Vec<u8>,
    visible: bool,
    complete: bool,
  },
}

struct PendingOutput {
  payload: PendingPayload,
  since: Instant,
}

impl PendingOutput {
  fn sequence(&self) -> u64 {
    match &self.payload {
      PendingPayload::Line(entry) | PendingPayload::Bytes { reference: entry, .. } => entry.sequence(),
    }
  }

  fn needs_tick(&self) -> bool {
    !matches!(self.payload, PendingPayload::Bytes { visible: true, .. })
  }

  fn into_entry(self) -> Option<ConsoleEntry> {
    match self.payload {
      PendingPayload::Line(entry) => Some(entry),
      PendingPayload::Bytes {
        reference,
        buffered,
        visible: false,
        ..
      } => Some(bytes_entry(&reference, buffered)),
      PendingPayload::Bytes { visible: true, .. } => None,
    }
  }
}

/// Emits the current logical stdout line after one second, or earlier at the bounded buffer limit.
/// Stderr and non-output records remain live.
pub struct TimedRenderer<R> {
  renderer: R,
  pending: HashMap<ConsoleScope, PendingOutput>,
}

impl<R> TimedRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self {
      renderer,
      pending: HashMap::new(),
    }
  }

  fn reveal(&mut self, scope: &ConsoleScope) -> Option<ConsoleEntry> {
    let mut pending = self.pending.remove(scope)?;
    match &mut pending.payload {
      PendingPayload::Line(entry) => Some(entry.clone()),
      PendingPayload::Bytes {
        reference,
        buffered,
        visible,
        complete,
      } => {
        if *visible || buffered.is_empty() {
          return None;
        }
        *visible = true;
        let entry = bytes_entry(reference, std::mem::take(buffered));
        if !*complete {
          self.pending.insert(scope.clone(), pending);
        }
        Some(entry)
      },
    }
  }

  fn render_bytes(&mut self, entry: &ConsoleEntry, scope: &ConsoleScope, bytes: &[u8]) -> io::Result<()>
  where
    R: ConsoleRenderer,
  {
    let mut ready = Vec::new();
    let mut current = self.pending.remove(scope);
    if current
      .as_ref()
      .is_some_and(|pending| matches!(pending.payload, PendingPayload::Line(_)))
    {
      let previous = current.take().expect("pending line was checked above");
      if previous.since.elapsed() >= VISIBILITY_THRESHOLD {
        if let Some(entry) = previous.into_entry() {
          ready.push(entry);
        }
      }
    }
    for segment in bytes.split_inclusive(|byte| *byte == b'\n') {
      let complete = segment.last() == Some(&b'\n');
      if current
        .as_ref()
        .is_some_and(|pending| matches!(pending.payload, PendingPayload::Bytes { complete: true, .. }))
      {
        let previous = current.take().expect("completed byte line was checked above");
        if previous.since.elapsed() >= VISIBILITY_THRESHOLD {
          if let Some(entry) = previous.into_entry() {
            ready.push(entry);
          }
        }
      }
      let mut pending = current.take().unwrap_or_else(|| PendingOutput {
        payload: PendingPayload::Bytes {
          reference: entry.clone(),
          buffered: Vec::new(),
          visible: false,
          complete: false,
        },
        since: Instant::now(),
      });
      let PendingPayload::Bytes {
        reference,
        buffered,
        visible,
        complete: pending_complete,
      } = &mut pending.payload
      else {
        unreachable!("only byte state is retained while processing a byte chunk")
      };
      if *visible {
        ready.push(bytes_entry(reference, segment.to_vec()));
      } else {
        buffered.extend_from_slice(segment);
        if buffered.len() >= MAX_PENDING_BYTES || pending.since.elapsed() >= VISIBILITY_THRESHOLD {
          *visible = true;
          ready.push(bytes_entry(reference, std::mem::take(buffered)));
        }
      }
      *pending_complete = complete;
      if !complete || !*visible {
        current = Some(pending);
      }
    }
    if let Some(pending) = current {
      self.pending.insert(scope.clone(), pending);
    }
    for entry in ready {
      self.renderer.render(&entry)?;
    }
    Ok(())
  }
}

fn bytes_entry(reference: &ConsoleEntry, bytes: Vec<u8>) -> ConsoleEntry {
  let ConsoleRecord::Execution(ExecutionEvent::Output {
    run_id,
    scope,
    step_id,
    command_id,
    stream,
    ..
  }) = reference.record()
  else {
    unreachable!("byte state is created only from output records")
  };
  reference.with_record(ConsoleRecord::Execution(ExecutionEvent::Output {
    run_id: *run_id,
    scope: scope.clone(),
    step_id: *step_id,
    command_id: command_id.clone(),
    stream: *stream,
    payload: ConsolePayload::Bytes(bytes),
  }))
}

impl<R: ConsoleRenderer> ConsoleRenderer for TimedRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    match entry.record() {
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Line(_),
        ..
      }) => {
        if let Some(previous) = self.pending.insert(
          scope.clone(),
          PendingOutput {
            payload: PendingPayload::Line(entry.clone()),
            since: Instant::now(),
          },
        ) {
          if previous.since.elapsed() < VISIBILITY_THRESHOLD {
            return Ok(());
          }
          if let Some(entry) = previous.into_entry() {
            self.renderer.render(&entry)?;
          }
        }
        Ok(())
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        stream: ConsoleStream::Stdout,
        payload: ConsolePayload::Bytes(bytes),
        ..
      }) => self.render_bytes(entry, scope, bytes),
      ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, .. }) => {
        if let Some(pending) = self.pending.remove(scope) {
          if pending.since.elapsed() >= VISIBILITY_THRESHOLD {
            if let Some(entry) = pending.into_entry() {
              self.renderer.render(&entry)?;
            }
          }
        }
        self.renderer.render(entry)
      },
      ConsoleRecord::Execution(ExecutionEvent::Output {
        scope: Some(scope),
        payload: ConsolePayload::RawBytes(_),
        ..
      }) => {
        self.pending.remove(scope);
        self.renderer.render(entry)
      },
      _ => self.renderer.render(entry),
    }
  }

  fn tick(&mut self) -> io::Result<()> {
    let mut ready = self
      .pending
      .iter()
      .filter(|(_, pending)| pending.needs_tick() && pending.since.elapsed() >= VISIBILITY_THRESHOLD)
      .map(|(scope, pending)| (pending.sequence(), scope.id(), scope.clone()))
      .collect::<Vec<_>>();
    ready.sort_by_key(|(sequence, scope_id, _)| (*sequence, *scope_id));
    for (_, _, scope) in ready {
      if let Some(entry) = self.reveal(&scope) {
        self.renderer.render(&entry)?;
      }
    }
    self.renderer.tick()
  }

  fn wants_tick(&self) -> bool {
    self.pending.values().any(PendingOutput::needs_tick) || self.renderer.wants_tick()
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.pending.remove(scope);
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.end_raw(scope)
  }

  fn supports_raw_terminal(&self) -> bool {
    self.renderer.supports_raw_terminal()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    self.renderer.set_parallel(parallel)
  }

  fn update_progress(&mut self, scope: &ConsoleScope, message: &str) -> io::Result<()> {
    self.renderer.update_progress(scope, message)
  }

  fn update_progress_bytes(
    &mut self,
    scope: &ConsoleScope,
    command_id: &str,
    stream: ConsoleStream,
    bytes: &[u8],
  ) -> io::Result<()> {
    self.renderer.update_progress_bytes(scope, command_id, stream, bytes)
  }

  fn supports_progress_updates(&self) -> bool {
    self.renderer.supports_progress_updates()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsoleRecord, ConsoleScopeAllocator, ConsoleStatus};

  #[derive(Default)]
  struct Recording(Vec<ConsoleRecord>);

  impl ConsoleRenderer for Recording {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.push(entry.record().clone());
      Ok(())
    }

    fn supports_raw_terminal(&self) -> bool {
      true
    }
  }

  fn line(scope: ConsoleScope, value: &str) -> ConsoleEntry {
    ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope),
      step_id: None,
      command_id: "command".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Line(value.to_owned()),
    }))
  }

  fn bytes(scope: ConsoleScope, value: &[u8]) -> ConsoleEntry {
    ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope),
      step_id: None,
      command_id: "command".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Bytes(value.to_vec()),
    }))
  }

  fn age_pending(renderer: &mut TimedRenderer<Recording>, scope: &ConsoleScope) {
    renderer.pending.get_mut(scope).unwrap().since = Instant::now() - VISIBILITY_THRESHOLD;
  }

  #[test]
  fn suppresses_short_lived_stdout_lines() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&line(scope.clone(), "one")).unwrap();
    renderer.render(&line(scope.clone(), "two")).unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope,
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();
    assert!(renderer
      .renderer
      .0
      .iter()
      .all(|record| !matches!(record, ConsoleRecord::Execution(ExecutionEvent::Output { .. }))));
  }

  #[test]
  fn tick_emits_a_line_that_remains_current_for_one_second() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    assert!(!renderer.wants_tick());
    renderer.render(&line(scope.clone(), "compiling")).unwrap();
    assert!(renderer.wants_tick());
    age_pending(&mut renderer, &scope);
    renderer.tick().unwrap();
    assert!(!renderer.wants_tick());
    assert!(matches!(
      renderer.renderer.0.as_slice(),
      [ConsoleRecord::Execution(ExecutionEvent::Output {
        payload: ConsolePayload::Line(line),
        ..
      })] if line == "compiling"
    ));
    assert!(renderer.pending.is_empty());
  }

  #[test]
  fn replaces_and_finishes_visible_pending_lines() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&line(scope.clone(), "one")).unwrap();
    age_pending(&mut renderer, &scope);
    renderer.render(&line(scope.clone(), "two")).unwrap();
    age_pending(&mut renderer, &scope);
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope,
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();

    let lines = renderer
      .renderer
      .0
      .iter()
      .filter_map(|record| match record {
        ConsoleRecord::Execution(ExecutionEvent::Output {
          payload: ConsolePayload::Line(line),
          ..
        }) => Some(line.as_str()),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(lines, ["one", "two"]);
  }

  #[test]
  fn raw_output_and_lifecycle_clear_pending_state() {
    let scope = ConsoleScopeAllocator::default().scope("raw");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&line(scope.clone(), "pending")).unwrap();
    let raw = ConsoleEntry::new(ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: Some(scope.clone()),
      step_id: None,
      command_id: "raw".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::RawBytes(b"raw".to_vec()),
    }));
    renderer.render(&raw).unwrap();
    assert!(renderer.pending.is_empty());

    renderer.render(&line(scope.clone(), "pending-again")).unwrap();
    renderer.begin_raw(&scope).unwrap();
    assert!(renderer.pending.is_empty());
    renderer.end_raw(&scope).unwrap();
    assert!(renderer.supports_raw_terminal());
    renderer.set_parallel(true).unwrap();
    renderer.update_progress(&scope, "working").unwrap();
    renderer
      .update_progress_bytes(&scope, "command", ConsoleStream::Stdout, b"partial")
      .unwrap();
  }

  #[test]
  fn reveals_an_unterminated_byte_line_and_streams_its_continuation() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&bytes(scope.clone(), b"compil")).unwrap();
    assert!(renderer.renderer.0.is_empty());

    age_pending(&mut renderer, &scope);
    renderer.tick().unwrap();
    renderer.render(&bytes(scope, b"ing\n")).unwrap();

    let output = renderer
      .renderer
      .0
      .iter()
      .filter_map(|record| match record {
        ConsoleRecord::Execution(ExecutionEvent::Output {
          payload: ConsolePayload::Bytes(bytes),
          ..
        }) => Some(bytes.as_slice()),
        _ => None,
      })
      .flatten()
      .copied()
      .collect::<Vec<_>>();
    assert_eq!(output, b"compiling\n");
    assert!(renderer.pending.is_empty());
  }

  #[test]
  fn a_complete_byte_line_remains_current_until_the_tick() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&bytes(scope.clone(), b"compiling\n")).unwrap();
    assert!(renderer.renderer.0.is_empty());

    age_pending(&mut renderer, &scope);
    renderer.tick().unwrap();

    assert!(matches!(
      renderer.renderer.0.as_slice(),
      [ConsoleRecord::Execution(ExecutionEvent::Output {
        payload: ConsolePayload::Bytes(bytes),
        ..
      })] if bytes == b"compiling\n"
    ));
    assert!(renderer.pending.is_empty());
  }

  #[test]
  fn byte_output_reveals_the_mature_line_it_replaces() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&line(scope.clone(), "first")).unwrap();
    age_pending(&mut renderer, &scope);
    renderer.render(&bytes(scope, b"second")).unwrap();

    assert!(matches!(
      renderer.renderer.0.as_slice(),
      [ConsoleRecord::Execution(ExecutionEvent::Output {
        payload: ConsolePayload::Line(line),
        ..
      })] if line == "first"
    ));
  }

  #[test]
  fn bounds_an_unterminated_line_by_revealing_large_output() {
    let scope = ConsoleScopeAllocator::default().scope("build");
    let mut renderer = TimedRenderer::new(Recording::default());
    renderer.render(&bytes(scope, &vec![b'x'; MAX_PENDING_BYTES])).unwrap();

    assert!(matches!(
      renderer.renderer.0.as_slice(),
      [ConsoleRecord::Execution(ExecutionEvent::Output {
        payload: ConsolePayload::Bytes(bytes),
        ..
      })] if bytes.len() == MAX_PENDING_BYTES
    ));
    assert!(renderer.pending.values().all(|pending| matches!(
      &pending.payload,
      PendingPayload::Bytes {
        buffered,
        visible: true,
        ..
      } if buffered.is_empty()
    )));
  }
}
