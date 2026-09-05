use std::{io, sync::Arc};

use indexmap::IndexMap;
use octa_output::{Console, ConsoleScope, ConsoleStatus, ExecutionEvent};
use tokio::sync::Mutex;

struct ScopeState {
  remaining: usize,
  status: ConsoleStatus,
  lifecycle: ScopeLifecycle,
}

impl Default for ScopeState {
  fn default() -> Self {
    Self {
      remaining: 0,
      status: ConsoleStatus::Skipped,
      lifecycle: ScopeLifecycle::Planned,
    }
  }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScopeLifecycle {
  Planned,
  Declared,
  Started,
  // Prevents concurrent terminal nodes from publishing duplicate finishes.
  // A failed publication restores the preceding state and can be retried.
  PublishingFinish,
  Finished,
}

struct PendingFinish {
  scope: ConsoleScope,
  status: ConsoleStatus,
  previous: ScopeLifecycle,
}

/// Tracks output-group completion separately from DAG scheduling and task payloads.
pub(crate) struct ConsoleScopeTracker {
  console: Arc<Console>,
  run_id: u64,
  states: Mutex<IndexMap<ConsoleScope, ScopeState>>,
}

impl ConsoleScopeTracker {
  pub(crate) fn new(
    console: Arc<Console>,
    run_id: u64,
    scopes: Vec<ConsoleScope>,
    node_scopes: impl IntoIterator<Item = ConsoleScope>,
  ) -> Self {
    let mut states = scopes
      .into_iter()
      .map(|scope| (scope, ScopeState::default()))
      .collect::<IndexMap<_, _>>();
    for scope in node_scopes {
      states.entry(scope).or_default().remaining += 1;
    }
    Self {
      console,
      run_id,
      states: Mutex::new(states),
    }
  }

  pub(crate) async fn declare(&self) -> io::Result<()> {
    let scopes = self.states.lock().await.keys().cloned().collect::<Vec<_>>();
    for scope in scopes {
      self
        .console
        .event(ExecutionEvent::ScopeDeclared {
          run_id: self.run_id,
          scope: scope.clone(),
        })
        .await?;
      let empty = {
        let mut states = self.states.lock().await;
        let state = states.get_mut(&scope).expect("scope was collected from the same map");
        state.lifecycle = ScopeLifecycle::Declared;
        state.remaining == 0
      };
      if empty {
        self.finish_scope(scope, ConsoleStatus::Skipped).await?;
      }
    }
    Ok(())
  }

  pub(crate) async fn start(&self, scope: &ConsoleScope) -> io::Result<()> {
    let mut states = self.states.lock().await;
    let Some(state) = states.get(scope) else {
      return Err(unknown_scope(scope));
    };
    match state.lifecycle {
      ScopeLifecycle::Started => return Ok(()),
      ScopeLifecycle::Declared => {},
      ScopeLifecycle::Planned => {
        return Err(io::Error::other(format!(
          "console scope {} was started before it was declared",
          scope.id()
        )));
      },
      ScopeLifecycle::PublishingFinish | ScopeLifecycle::Finished => {
        return Err(io::Error::other(format!(
          "console scope {} was started after it finished",
          scope.id()
        )));
      },
    }

    // Keep the state lock until the event is accepted so concurrent nodes in
    // one invocation cannot publish duplicate starts.
    self
      .console
      .event(ExecutionEvent::ScopeStarted {
        run_id: self.run_id,
        scope: scope.clone(),
      })
      .await?;
    states.get_mut(scope).expect("scope was validated above").lifecycle = ScopeLifecycle::Started;
    Ok(())
  }

  pub(crate) async fn complete(&self, scope: &ConsoleScope, status: ConsoleStatus) -> io::Result<()> {
    let finished = {
      let mut states = self.states.lock().await;
      let Some(state) = states.get_mut(scope) else {
        return Err(unknown_scope(scope));
      };
      if state.lifecycle != ScopeLifecycle::Started {
        return Err(io::Error::other(format!(
          "console scope {} was completed before it started",
          scope.id()
        )));
      }
      if state.remaining == 0 {
        return Err(io::Error::other(format!(
          "console scope {} was completed more than once",
          scope.id()
        )));
      }
      state.status = state.status.max(status);
      state.remaining -= 1;
      if state.remaining == 0 {
        state.lifecycle = ScopeLifecycle::PublishingFinish;
        Some(PendingFinish {
          scope: scope.clone(),
          status: state.status,
          previous: ScopeLifecycle::Started,
        })
      } else {
        None
      }
    };
    if let Some(finish) = finished {
      self.publish_finish(finish).await?;
    }
    Ok(())
  }

  pub(crate) async fn finish_remaining(&self, status: ConsoleStatus) -> io::Result<()> {
    let unfinished = {
      let mut states = self.states.lock().await;
      states
        .iter_mut()
        .filter(|(_, state)| matches!(state.lifecycle, ScopeLifecycle::Declared | ScopeLifecycle::Started))
        .map(|(scope, state)| {
          let previous = state.lifecycle;
          state.lifecycle = ScopeLifecycle::PublishingFinish;
          state.status = state.status.max(status);
          PendingFinish {
            scope: scope.clone(),
            status: state.status,
            previous,
          }
        })
        .collect::<Vec<_>>()
    };
    let mut first_error = None;
    for finish in unfinished {
      if let Err(error) = self.publish_finish(finish).await {
        if first_error.is_none() {
          first_error = Some(error);
        }
      }
    }
    first_error.map_or(Ok(()), Err)
  }

  async fn finish_scope(&self, scope: ConsoleScope, status: ConsoleStatus) -> io::Result<()> {
    let finish = {
      let mut states = self.states.lock().await;
      let state = states.get_mut(&scope).ok_or_else(|| unknown_scope(&scope))?;
      let previous = state.lifecycle;
      state.lifecycle = ScopeLifecycle::PublishingFinish;
      state.status = state.status.max(status);
      PendingFinish {
        scope,
        status: state.status,
        previous,
      }
    };
    self.publish_finish(finish).await
  }

  async fn publish_finish(&self, finish: PendingFinish) -> io::Result<()> {
    let result = self
      .console
      .event(ExecutionEvent::ScopeFinished {
        run_id: self.run_id,
        scope: finish.scope.clone(),
        status: finish.status,
      })
      .await;
    let mut states = self.states.lock().await;
    let state = states.get_mut(&finish.scope).expect("scope was validated above");
    state.lifecycle = if result.is_ok() {
      ScopeLifecycle::Finished
    } else {
      finish.previous
    };
    result
  }

  pub(crate) async fn successful_run_status(&self) -> ConsoleStatus {
    let states = self.states.lock().await;
    if states.is_empty() {
      return ConsoleStatus::Success;
    }
    states
      .values()
      .fold(ConsoleStatus::Skipped, |status, state| status.max(state.status))
  }
}

fn unknown_scope(scope: &ConsoleScope) -> io::Error {
  io::Error::other(format!("unknown console scope {}", scope.id()))
}

#[cfg(test)]
mod tests {
  use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex as StdMutex,
  };

  use octa_output::{ConsoleEntry, ConsoleRecord, ConsoleRenderer};

  use super::*;

  #[derive(Clone, Default)]
  struct RecordingRenderer(Arc<StdMutex<Vec<ConsoleRecord>>>);

  impl ConsoleRenderer for RecordingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.lock().unwrap().push(entry.record().clone());
      Ok(())
    }
  }

  fn recording_console() -> (Arc<Console>, RecordingRenderer) {
    let renderer = RecordingRenderer::default();
    let console = Arc::new(Console::new(renderer.clone()));
    (console, renderer)
  }

  #[derive(Clone)]
  struct RejectFirstFinish {
    records: Arc<StdMutex<Vec<ConsoleRecord>>>,
    reject: Arc<AtomicBool>,
  }

  impl ConsoleRenderer for RejectFirstFinish {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.records.lock().unwrap().push(entry.record().clone());
      if matches!(
        entry.record(),
        ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { .. })
      ) && self.reject.swap(false, Ordering::SeqCst)
      {
        return Err(io::Error::other("finish rejected"));
      }
      Ok(())
    }
  }

  #[tokio::test]
  async fn closes_completed_and_empty_scopes() {
    let (console, renderer) = recording_console();
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let first = allocator.scope("first");
    let empty = allocator.scope("empty");
    let tracker = ConsoleScopeTracker::new(console, 7, vec![first.clone(), empty.clone()], vec![first.clone()]);

    tracker.declare().await.unwrap();
    tracker.start(&first).await.unwrap();
    tracker.complete(&first, ConsoleStatus::Success).await.unwrap();
    tracker.finish_remaining(ConsoleStatus::Failed).await.unwrap();

    let events = renderer.0.lock().unwrap();
    assert_eq!(
      events
        .iter()
        .filter_map(|record| match record {
          ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared { scope, .. }) => Some(scope),
          _ => None,
        })
        .collect::<Vec<_>>(),
      vec![&first, &empty]
    );
    assert!(events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeStarted {
      run_id: 7,
      scope: first.clone(),
    })));
    assert!(
      !events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeStarted {
        run_id: 7,
        scope: empty.clone(),
      }))
    );
    assert!(
      events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
        run_id: 7,
        scope: first,
        status: ConsoleStatus::Success,
      }))
    );
    assert!(
      events.contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
        run_id: 7,
        scope: empty,
        status: ConsoleStatus::Skipped,
      }))
    );
  }

  #[tokio::test]
  async fn validates_lifecycle_and_aggregates_node_statuses() {
    let (console, renderer) = recording_console();
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let unknown = allocator.scope("unknown");
    let tracker = ConsoleScopeTracker::new(console, 8, Vec::new(), vec![scope.clone(), scope.clone()]);

    assert!(tracker.complete(&scope, ConsoleStatus::Success).await.is_err());
    assert!(tracker.complete(&unknown, ConsoleStatus::Success).await.is_err());

    tracker.declare().await.unwrap();
    tracker.start(&scope).await.unwrap();
    tracker.complete(&scope, ConsoleStatus::Success).await.unwrap();
    tracker.complete(&scope, ConsoleStatus::Skipped).await.unwrap();

    assert_eq!(tracker.successful_run_status().await, ConsoleStatus::Success);
    assert!(tracker.complete(&scope, ConsoleStatus::Success).await.is_err());
    assert!(renderer
      .0
      .lock()
      .unwrap()
      .contains(&ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
        run_id: 8,
        scope,
        status: ConsoleStatus::Success,
      })));
  }

  #[tokio::test]
  async fn retries_failed_finishes_without_skipping_later_scopes() {
    let records = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RejectFirstFinish {
      records: records.clone(),
      reject: Arc::new(AtomicBool::new(true)),
    }));
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let first = allocator.scope("first");
    let second = allocator.scope("second");
    let tracker = ConsoleScopeTracker::new(
      console,
      9,
      vec![first.clone(), second.clone()],
      vec![first.clone(), second.clone()],
    );

    tracker.declare().await.unwrap();
    tracker.start(&first).await.unwrap();
    tracker.start(&second).await.unwrap();
    assert!(tracker.finish_remaining(ConsoleStatus::Failed).await.is_err());

    let first_pass = records
      .lock()
      .unwrap()
      .iter()
      .filter(|record| matches!(record, ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { .. })))
      .count();
    assert_eq!(first_pass, 2);

    tracker.finish_remaining(ConsoleStatus::Failed).await.unwrap();
    let finishes = records
      .lock()
      .unwrap()
      .iter()
      .filter_map(|record| match record {
        ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { scope, .. }) => Some(scope.clone()),
        _ => None,
      })
      .collect::<Vec<_>>();
    assert_eq!(finishes.iter().filter(|scope| *scope == &first).count(), 2);
    assert_eq!(finishes.iter().filter(|scope| *scope == &second).count(), 1);
  }
}
