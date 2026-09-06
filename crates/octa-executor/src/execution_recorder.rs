//! Transactional task and step lifecycle recording.
//!
//! The recorder is the single authority for lifecycle ordering and result
//! aggregation. State changes become visible only after the corresponding
//! event is accepted; rejected finish events restore their prior state and may
//! therefore be retried during shutdown.
//!
//! A task scope can contain several DAG nodes, but a step represents one plugin
//! command. Scope completion is therefore reference-counted, while each step
//! has a single terminal transition. Normal lifecycle follows `Planned ->
//! Declared -> Started -> PublishingFinish -> Finished`; shutdown may finish a
//! declared step directly when it was cancelled before scheduling.

use std::{collections::HashMap, io, sync::Arc};

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use octa_output::{Console, ConsoleScope, ConsoleStatus, ConsoleStep, ExecutionEvent};
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::{
  execution_result::{conclusion, ExecutionFailure, OutputReference, StepResult, TaskResult, TaskRole},
  task::ExecutionBinding,
};

/// Aggregated state for all graph nodes belonging to one task invocation.
struct ScopeState {
  /// Bound DAG nodes that have not reported a terminal outcome.
  remaining: usize,
  /// Most severe outcome observed across completed nodes.
  status: ConsoleStatus,
  /// Current externally visible lifecycle transition.
  lifecycle: LifecycleState,
  /// Time at which the first accepted start event was published.
  started_at: Option<DateTime<Utc>>,
  /// Time at which the accepted finish event was published.
  finished_at: Option<DateTime<Utc>>,
  /// First originating failure, with cancellation replaceable by a later cause.
  failure: Option<ExecutionFailure>,
  /// Observation time used to select the run's originating failure.
  failure_at: Option<DateTime<Utc>>,
}

impl Default for ScopeState {
  fn default() -> Self {
    Self {
      remaining: 0,
      status: ConsoleStatus::Skipped,
      lifecycle: LifecycleState::Planned,
      started_at: None,
      finished_at: None,
      failure: None,
      failure_at: None,
    }
  }
}

#[derive(Clone, Copy, Eq, PartialEq)]
/// Publication-aware lifecycle used to make terminal events retryable.
enum LifecycleState {
  /// Identity exists internally but has not been announced to observers.
  Planned,
  /// Declaration was accepted; execution has not started.
  Declared,
  /// Start was accepted and one or more bound nodes may be running.
  Started,
  /// A finish event is in flight outside the state lock.
  ///
  /// This reservation prevents concurrent terminal nodes from publishing a
  /// duplicate finish. Failed publication restores the preceding state.
  PublishingFinish,
  /// Terminal event was accepted and timestamps/result data are complete.
  Finished,
}

/// Scope event reserved under the state lock and published after releasing it.
struct PendingScopeFinish {
  /// Scope whose state was reserved for publication.
  scope: ConsoleScope,
  /// Aggregated terminal status included in the event.
  status: ConsoleStatus,
  /// State restored when the output sink rejects the event.
  previous: LifecycleState,
}

/// Step event reserved under the state lock and published after releasing it.
struct PendingStepFinish {
  /// Step whose state was reserved for publication.
  step: ConsoleStep,
  /// Owning task scope required by the public event hierarchy.
  scope: ConsoleScope,
  /// Terminal status included in the event.
  status: ConsoleStatus,
  /// State restored when the output sink rejects the event.
  previous: LifecycleState,
}

/// Lifecycle and failure information for one executable command step.
struct StepState {
  /// Task invocation containing this command step.
  scope: ConsoleScope,
  /// Current externally visible lifecycle transition.
  lifecycle: LifecycleState,
  /// Terminal status, initialized as skipped until execution begins.
  status: ConsoleStatus,
  /// Time at which the start event was accepted, if the step was scheduled.
  started_at: Option<DateTime<Utc>>,
  /// Time at which the finish event was accepted.
  finished_at: Option<DateTime<Utc>>,
  /// Failure attached to the step's terminal result.
  failure: Option<ExecutionFailure>,
  /// Structured values returned by the step's plugin operation.
  outputs: Map<String, Value>,
}

/// Tracks task/step lifecycle separately from DAG scheduling and task payloads.
///
/// The scheduler reports transitions through [`ExecutionBinding`]; the recorder
/// emits ordered events and later materializes [`TaskResult`] values from the
/// same state, preventing event and terminal-result models from diverging.
pub(crate) struct ExecutionRecorder {
  /// Single asynchronous output boundary for lifecycle publication.
  console: Arc<Console>,
  /// Correlation ID attached to every emitted event and output reference.
  run_id: u64,
  /// Serializes retryable declaration passes without holding the state lock over I/O.
  declaration: Mutex<()>,
  /// Prevents duplicate starts when several nodes share one task scope.
  starts: Mutex<()>,
  /// Task states kept in declaration order for deterministic results.
  states: Mutex<IndexMap<ConsoleScope, ScopeState>>,
  /// Step states kept in DAG/binding declaration order.
  steps: Mutex<IndexMap<ConsoleStep, StepState>>,
}

impl ExecutionRecorder {
  /// Creates lifecycle state from declared scopes and executable-node bindings.
  ///
  /// `scopes` determines stable result order. Bindings provide the number of
  /// nodes that must complete each scope and declare the command steps nested
  /// below those scopes.
  pub(crate) fn new(
    console: Arc<Console>,
    run_id: u64,
    scopes: Vec<ConsoleScope>,
    node_bindings: impl IntoIterator<Item = ExecutionBinding>,
  ) -> Self {
    let mut states = scopes
      .into_iter()
      .map(|scope| (scope, ScopeState::default()))
      .collect::<IndexMap<_, _>>();
    let mut steps = IndexMap::new();
    for binding in node_bindings {
      let scope = binding.scope().clone();
      // Several internal or executable nodes may contribute to one invocation.
      // The scope finishes only after all of them report completion.
      states.entry(scope.clone()).or_default().remaining += 1;
      if let Some(step) = binding.step() {
        steps.insert(
          step.clone(),
          StepState {
            scope,
            lifecycle: LifecycleState::Planned,
            status: ConsoleStatus::Skipped,
            started_at: None,
            finished_at: None,
            failure: None,
            outputs: Map::new(),
          },
        );
      }
    }
    Self {
      console,
      run_id,
      declaration: Mutex::new(()),
      starts: Mutex::new(()),
      states: Mutex::new(states),
      steps: Mutex::new(steps),
    }
  }

  /// Publishes every still-planned scope and step in deterministic order.
  ///
  /// The method is retryable and idempotent for already accepted declarations.
  /// Empty scopes are immediately closed as skipped so every declared identity
  /// still receives a terminal event and result.
  pub(crate) async fn declare(&self) -> io::Result<()> {
    let _declaration = self.declaration.lock().await;

    // Clone a snapshot before awaiting the sink. This avoids holding state
    // locks during potentially backpressured output publication.
    let scopes = self
      .states
      .lock()
      .await
      .iter()
      .filter(|(_, state)| state.lifecycle == LifecycleState::Planned)
      .map(|(scope, _)| scope.clone())
      .collect::<Vec<_>>();
    for scope in scopes {
      self
        .console
        .event(ExecutionEvent::ScopeDeclared {
          run_id: self.run_id,
          scope: scope.clone(),
        })
        .await?;
      // Commit the state transition only after the event sink accepts it.
      let empty = {
        let mut states = self.states.lock().await;
        let state = states.get_mut(&scope).expect("scope was collected from the same map");
        state.lifecycle = LifecycleState::Declared;
        state.remaining == 0
      };
      if empty {
        self.finish_scope(scope, ConsoleStatus::Skipped).await?;
      }
    }
    // Scopes are declared before their children so observers can construct the
    // hierarchy incrementally from the event stream.
    let steps = self
      .steps
      .lock()
      .await
      .iter()
      .filter(|(_, state)| state.lifecycle == LifecycleState::Planned)
      .map(|(step, state)| (step.clone(), state.scope.clone()))
      .collect::<Vec<_>>();
    for (step, scope) in steps {
      self
        .console
        .event(ExecutionEvent::StepDeclared {
          run_id: self.run_id,
          scope,
          step: step.clone(),
        })
        .await?;
      self
        .steps
        .lock()
        .await
        .get_mut(&step)
        .expect("step was collected from the same map")
        .lifecycle = LifecycleState::Declared;
    }
    Ok(())
  }

  /// Starts a task scope once, even when multiple nodes enter it concurrently.
  ///
  /// Validation happens before publication, while the state and timestamp are
  /// committed only after the sink accepts `ScopeStarted`.
  pub(crate) async fn start_scope(&self, binding: &ExecutionBinding) -> io::Result<()> {
    let scope = binding.scope();
    let _start = self.starts.lock().await;
    {
      let states = self.states.lock().await;
      let state = states.get(scope).ok_or_else(|| unknown_scope(scope))?;
      match state.lifecycle {
        LifecycleState::Declared => {},
        LifecycleState::Started => return Ok(()),
        LifecycleState::Planned => {
          return Err(io::Error::other(format!(
            "execution scope {} was started before it was declared",
            scope.id()
          )));
        },
        LifecycleState::PublishingFinish | LifecycleState::Finished => {
          return Err(io::Error::other(format!(
            "execution scope {} was started after it finished",
            scope.id()
          )));
        },
      }
    };
    self
      .console
      .event(ExecutionEvent::ScopeStarted {
        run_id: self.run_id,
        scope: scope.clone(),
      })
      .await?;
    let mut states = self.states.lock().await;
    let state = states.get_mut(scope).expect("scope was validated above");
    state.lifecycle = LifecycleState::Started;
    state.started_at = Some(Utc::now());
    Ok(())
  }

  /// Starts the command step attached to a binding, if it has one.
  ///
  /// Task-only bindings intentionally make this a no-op. The shared start lock
  /// preserves scope/step ordering when sibling nodes begin concurrently.
  pub(crate) async fn start_step(&self, binding: &ExecutionBinding) -> io::Result<()> {
    let Some(step) = binding.step() else {
      return Ok(());
    };
    let _start = self.starts.lock().await;
    let scope = {
      let steps = self.steps.lock().await;
      let state = steps.get(step).ok_or_else(|| unknown_step(step))?;
      match state.lifecycle {
        LifecycleState::Declared => state.scope.clone(),
        LifecycleState::Started => return Ok(()),
        LifecycleState::Planned => {
          return Err(io::Error::other(format!(
            "execution step {} was started before it was declared",
            step.id()
          )));
        },
        LifecycleState::PublishingFinish | LifecycleState::Finished => {
          return Err(io::Error::other(format!(
            "execution step {} was started after it finished",
            step.id()
          )));
        },
      }
    };
    self
      .console
      .event(ExecutionEvent::StepStarted {
        run_id: self.run_id,
        scope,
        step: step.clone(),
      })
      .await?;
    let mut steps = self.steps.lock().await;
    let state = steps.get_mut(step).expect("step was validated above");
    state.lifecycle = LifecycleState::Started;
    state.started_at = Some(Utc::now());
    Ok(())
  }

  /// Records one bound node's outcome and closes its step/scope when eligible.
  ///
  /// Step completion is published before task completion to preserve hierarchy.
  /// A scope aggregates status by severity and finishes only when `remaining`
  /// reaches zero. No scope state is consumed if step publication fails.
  pub(crate) async fn complete(
    &self,
    binding: &ExecutionBinding,
    status: ConsoleStatus,
    failure: Option<ExecutionFailure>,
    outputs: Map<String, Value>,
  ) -> io::Result<()> {
    let scope = binding.scope();
    let failure_at = failure.as_ref().map(|_| Utc::now());
    {
      // Validate the parent first. Otherwise an invalid binding could publish a
      // valid-looking step finish and fail only when updating its task scope.
      let states = self.states.lock().await;
      let Some(state) = states.get(scope) else {
        return Err(unknown_scope(scope));
      };
      if state.lifecycle != LifecycleState::Started {
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
    }
    self
      .finish_step(binding.step(), status, failure.clone(), outputs)
      .await?;
    let finished = {
      let mut states = self.states.lock().await;
      let Some(state) = states.get_mut(scope) else {
        return Err(unknown_scope(scope));
      };
      if state.lifecycle != LifecycleState::Started {
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
      // ConsoleStatus ordering is severity ordering, so one failed node keeps
      // the complete invocation failed regardless of later successes/skips.
      state.status = state.status.max(status);
      record_timed_failure(&mut state.failure, &mut state.failure_at, failure, failure_at);
      state.remaining -= 1;
      if state.remaining == 0 {
        state.lifecycle = LifecycleState::PublishingFinish;
        Some(PendingScopeFinish {
          scope: scope.clone(),
          status: state.status,
          previous: LifecycleState::Started,
        })
      } else {
        None
      }
    };
    if let Some(finish) = finished {
      self.publish_scope_finish(finish).await?;
    }
    Ok(())
  }

  /// Closes every unfinished step and scope during cancellation or early failure.
  ///
  /// All entries are attempted even if one event is rejected; the first sink
  /// error is returned after the remaining lifecycle has had a chance to close.
  /// Children are always finished before their owning task scopes.
  pub(crate) async fn finish_remaining(&self, status: ConsoleStatus) -> io::Result<()> {
    let mut first_error = self.finish_remaining_steps(status).await.err();
    let unfinished = {
      let mut states = self.states.lock().await;
      states
        .iter_mut()
        .filter(|(_, state)| matches!(state.lifecycle, LifecycleState::Declared | LifecycleState::Started))
        .map(|(scope, state)| {
          let previous = state.lifecycle;
          state.lifecycle = LifecycleState::PublishingFinish;
          let terminal_status = remaining_status(status, previous);
          state.status = state.status.max(terminal_status);
          PendingScopeFinish {
            scope: scope.clone(),
            status: state.status,
            previous,
          }
        })
        .collect::<Vec<_>>()
    };
    for finish in unfinished {
      if let Err(error) = self.publish_scope_finish(finish).await {
        if first_error.is_none() {
          first_error = Some(error);
        }
      }
    }
    first_error.map_or(Ok(()), Err)
  }

  /// Reserves and publishes the normal terminal transition of one step.
  async fn finish_step(
    &self,
    step: Option<&ConsoleStep>,
    status: ConsoleStatus,
    failure: Option<ExecutionFailure>,
    outputs: Map<String, Value>,
  ) -> io::Result<()> {
    let Some(step) = step else {
      return Ok(());
    };
    let finish = {
      let mut steps = self.steps.lock().await;
      let state = steps.get_mut(step).ok_or_else(|| unknown_step(step))?;
      if !matches!(state.lifecycle, LifecycleState::Declared | LifecycleState::Started) {
        return Err(io::Error::other(format!(
          "console step {} was completed outside its active lifecycle",
          step.id()
        )));
      }
      let previous = state.lifecycle;
      state.lifecycle = LifecycleState::PublishingFinish;
      state.status = state.status.max(status);
      record_failure(&mut state.failure, failure);
      state.outputs = outputs;
      PendingStepFinish {
        step: step.clone(),
        scope: state.scope.clone(),
        status: state.status,
        previous,
      }
    };
    self.publish_step_finish(finish).await
  }

  /// Attempts shutdown completion for every declared or started step.
  async fn finish_remaining_steps(&self, status: ConsoleStatus) -> io::Result<()> {
    let unfinished = {
      let mut steps = self.steps.lock().await;
      steps
        .iter_mut()
        .filter(|(_, state)| matches!(state.lifecycle, LifecycleState::Declared | LifecycleState::Started))
        .map(|(step, state)| {
          let previous = state.lifecycle;
          state.lifecycle = LifecycleState::PublishingFinish;
          let terminal_status = remaining_status(status, previous);
          state.status = state.status.max(terminal_status);
          PendingStepFinish {
            step: step.clone(),
            scope: state.scope.clone(),
            status: state.status,
            previous,
          }
        })
        .collect::<Vec<_>>()
    };
    let mut first_error = None;
    for finish in unfinished {
      if let Err(error) = self.publish_step_finish(finish).await {
        first_error.get_or_insert(error);
      }
    }
    first_error.map_or(Ok(()), Err)
  }

  /// Finishes a scope that has no bound node, such as an empty invocation.
  async fn finish_scope(&self, scope: ConsoleScope, status: ConsoleStatus) -> io::Result<()> {
    let finish = {
      let mut states = self.states.lock().await;
      let state = states.get_mut(&scope).ok_or_else(|| unknown_scope(&scope))?;
      let previous = state.lifecycle;
      state.lifecycle = LifecycleState::PublishingFinish;
      state.status = state.status.max(status);
      PendingScopeFinish {
        scope,
        status: state.status,
        previous,
      }
    };
    self.publish_scope_finish(finish).await
  }

  /// Publishes a reserved scope finish and commits or rolls back its state.
  async fn publish_scope_finish(&self, finish: PendingScopeFinish) -> io::Result<()> {
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
    if result.is_ok() {
      state.lifecycle = LifecycleState::Finished;
      state.finished_at = Some(Utc::now());
    } else {
      state.lifecycle = finish.previous;
    }
    result
  }

  /// Publishes a reserved step finish and commits or rolls back its state.
  async fn publish_step_finish(&self, finish: PendingStepFinish) -> io::Result<()> {
    let result = self
      .console
      .event(ExecutionEvent::StepFinished {
        run_id: self.run_id,
        scope: finish.scope,
        step: finish.step.clone(),
        status: finish.status,
      })
      .await;
    let mut steps = self.steps.lock().await;
    let state = steps.get_mut(&finish.step).expect("step was validated above");
    if result.is_ok() {
      state.lifecycle = LifecycleState::Finished;
      state.finished_at = Some(Utc::now());
    } else {
      state.lifecycle = finish.previous;
    }
    result
  }

  /// Materializes the terminal result tree after every lifecycle has finished.
  ///
  /// Output bytes remain in the event stream; results contain stable selectors
  /// that an embedding application can use to retrieve or persist that output.
  /// Calling this before terminal publication is rejected rather than returning
  /// a misleading partial snapshot.
  pub(crate) async fn results(&self) -> io::Result<Vec<TaskResult>> {
    let steps = self.steps.lock().await;
    let mut steps_by_scope = HashMap::new();
    for (step, state) in steps.iter() {
      let finished_at = state
        .finished_at
        .ok_or_else(|| io::Error::other(format!("console step {} has no terminal result", step.id())))?;
      steps_by_scope
        .entry(state.scope.id())
        .or_insert_with(Vec::new)
        .push(StepResult {
          step_id: step.id(),
          label: step.label().to_owned(),
          started_at: state.started_at,
          finished_at,
          conclusion: conclusion(
            state.status,
            state.failure.clone(),
            Some(state.scope.id()),
            Some(step.id()),
          ),
          output: OutputReference::step(self.run_id, state.scope.id(), step.id()),
          outputs: state.outputs.clone(),
        });
    }
    drop(steps);

    // Group steps first, then consume them while walking task scopes in their
    // original declaration order.
    let states = self.states.lock().await;
    states
      .iter()
      .map(|(scope, state)| {
        let finished_at = state
          .finished_at
          .ok_or_else(|| io::Error::other(format!("console scope {} has no terminal result", scope.id())))?;
        Ok(TaskResult {
          task_id: scope.id(),
          parent_task_id: scope.parent_task_id(),
          label: scope.label().to_owned(),
          role: TaskRole::Main,
          started_at: state.started_at,
          finished_at,
          conclusion: conclusion(state.status, state.failure.clone(), Some(scope.id()), None),
          output: OutputReference::task(self.run_id, scope.id()),
          steps: steps_by_scope.remove(&scope.id()).unwrap_or_default(),
        })
      })
      .collect()
  }

  /// Aggregates a run status when scheduling itself produced no failure.
  ///
  /// An empty plan is successful; a non-empty plan consisting only of skipped
  /// scopes remains skipped.
  pub(crate) async fn successful_run_status(&self) -> ConsoleStatus {
    let states = self.states.lock().await;
    if states.is_empty() {
      return ConsoleStatus::Success;
    }
    states
      .values()
      .fold(ConsoleStatus::Skipped, |status, state| status.max(state.status))
  }

  /// Returns the earliest originating task failure observed in the run.
  ///
  /// Parallel fail-fast commonly records sibling cancellations before the task
  /// that caused them completes. Non-cancellation failures therefore take
  /// priority; cancellation is returned only when no originating failure exists.
  pub(crate) async fn failure(&self) -> Option<ExecutionFailure> {
    let states = self.states.lock().await;
    let first_failure = states
      .values()
      .filter_map(|state| Some((state.failure_at?, state.failure.as_ref()?)))
      .filter(|(_, failure)| failure.kind != crate::ExecutionFailureKind::Cancelled)
      .min_by_key(|(recorded_at, _)| *recorded_at)
      .map(|(_, failure)| failure);
    first_failure
      .or_else(|| {
        states
          .values()
          .filter_map(|state| Some((state.failure_at?, state.failure.as_ref()?)))
          .min_by_key(|(recorded_at, _)| *recorded_at)
          .map(|(_, failure)| failure)
      })
      .cloned()
  }
}

/// Maps run shutdown status onto a scope or step that never started.
///
/// A sibling's failure makes queued work skipped, not failed. Cancellation is
/// retained because it directly explains why queued work did not start.
fn remaining_status(status: ConsoleStatus, lifecycle: LifecycleState) -> ConsoleStatus {
  if status == ConsoleStatus::Failed && lifecycle == LifecycleState::Declared {
    ConsoleStatus::Skipped
  } else {
    status
  }
}

/// Records failure and observation time using originating-error precedence.
fn record_timed_failure(
  recorded: &mut Option<ExecutionFailure>,
  recorded_at: &mut Option<DateTime<Utc>>,
  failure: Option<ExecutionFailure>,
  failure_at: Option<DateTime<Utc>>,
) {
  let Some(failure) = failure else {
    return;
  };
  if should_record_failure(recorded.as_ref(), &failure) {
    *recorded = Some(failure);
    *recorded_at = failure_at;
  }
}

/// Records a step failure using the same precedence as task aggregation.
fn record_failure(recorded: &mut Option<ExecutionFailure>, failure: Option<ExecutionFailure>) {
  let Some(failure) = failure else {
    return;
  };
  if should_record_failure(recorded.as_ref(), &failure) {
    *recorded = Some(failure);
  }
}

/// Keeps the first useful failure, allowing a cause to replace prior cancellation.
fn should_record_failure(recorded: Option<&ExecutionFailure>, failure: &ExecutionFailure) -> bool {
  recorded.is_none()
    || recorded.is_some_and(|failure| failure.kind == crate::ExecutionFailureKind::Cancelled)
      && failure.kind != crate::ExecutionFailureKind::Cancelled
}

/// Creates a consistent error for a binding owned by another recorder.
fn unknown_scope(scope: &ConsoleScope) -> io::Error {
  io::Error::other(format!("unknown console scope {}", scope.id()))
}

/// Creates a consistent error for an undeclared step identity.
fn unknown_step(step: &ConsoleStep) -> io::Error {
  io::Error::other(format!("unknown console step {}", step.id()))
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

  #[derive(Clone)]
  struct RejectFirstStepFinish {
    records: Arc<StdMutex<Vec<ConsoleRecord>>>,
    reject: Arc<AtomicBool>,
  }

  impl ConsoleRenderer for RejectFirstStepFinish {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.records.lock().unwrap().push(entry.record().clone());
      if matches!(
        entry.record(),
        ConsoleRecord::Execution(ExecutionEvent::StepFinished { .. })
      ) && self.reject.swap(false, Ordering::SeqCst)
      {
        return Err(io::Error::other("step finish rejected"));
      }
      Ok(())
    }
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
    let binding = ExecutionBinding::for_task(first.clone());
    let tracker = ExecutionRecorder::new(console, 7, vec![first.clone(), empty.clone()], [binding.clone()]);

    tracker.declare().await.unwrap();
    tracker.start_scope(&binding).await.unwrap();
    tracker.start_step(&binding).await.unwrap();
    tracker
      .complete(&binding, ConsoleStatus::Success, None, Default::default())
      .await
      .unwrap();
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
  async fn builds_ordered_task_and_step_results_from_lifecycle_state() {
    let (console, _) = recording_console();
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let parent = allocator.scope("build");
    let child = allocator.scope_with_parent_options("compile", Some(parent.id()), None, false, false);
    let step = allocator.step(&child, "shell");
    let binding = ExecutionBinding::for_step(child.clone(), step.clone());
    let tracker = ExecutionRecorder::new(console, 19, vec![parent.clone(), child.clone()], [binding.clone()]);

    tracker.declare().await.unwrap();
    tracker.start_scope(&binding).await.unwrap();
    tracker.start_step(&binding).await.unwrap();
    let failure = ExecutionFailure::from_error(
      &crate::error::ExecutorError::TaskFailed("compile".to_owned()),
      Some(&binding),
    );
    let outputs = serde_json::Map::from_iter([("digest".to_owned(), serde_json::json!("sha256:test"))]);
    tracker
      .complete(&binding, ConsoleStatus::Failed, Some(failure.clone()), outputs.clone())
      .await
      .unwrap();
    tracker.finish_remaining(ConsoleStatus::Failed).await.unwrap();

    let results = tracker.results().await.unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0].task_id, parent.id());
    assert_eq!(results[0].started_at, None);
    assert_eq!(results[0].conclusion, crate::ExecutionConclusion::Skipped);
    assert_eq!(results[1].task_id, child.id());
    assert_eq!(results[1].parent_task_id, Some(parent.id()));
    assert!(results[1].started_at.is_some());
    assert_eq!(results[1].conclusion.failure(), Some(&failure));
    assert_eq!(results[1].output, OutputReference::task(19, child.id()));
    assert_eq!(results[1].steps.len(), 1);
    assert_eq!(results[1].steps[0].step_id, step.id());
    assert_eq!(results[1].steps[0].conclusion.failure(), Some(&failure));
    assert_eq!(results[1].steps[0].outputs, outputs);
    assert_eq!(
      results[1].steps[0].output,
      OutputReference::step(19, child.id(), step.id())
    );
  }

  #[tokio::test]
  async fn run_failure_uses_failure_observation_order_not_declaration_order() {
    let (console, _) = recording_console();
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let declared_first = allocator.scope("first");
    let failed_first = allocator.scope("second");
    let first_binding = ExecutionBinding::for_task(declared_first.clone());
    let second_binding = ExecutionBinding::for_task(failed_first.clone());
    let tracker = ExecutionRecorder::new(
      console,
      23,
      vec![declared_first.clone(), failed_first.clone()],
      [first_binding.clone(), second_binding.clone()],
    );
    tracker.declare().await.unwrap();
    tracker.start_scope(&first_binding).await.unwrap();
    tracker.start_scope(&second_binding).await.unwrap();
    let second_failure = ExecutionFailure::synthetic(
      crate::ExecutionFailureKind::Task,
      "second failed first",
      Some(failed_first.id()),
      None,
    );
    tracker
      .complete(
        &second_binding,
        ConsoleStatus::Failed,
        Some(second_failure.clone()),
        Default::default(),
      )
      .await
      .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    let first_failure = ExecutionFailure::synthetic(
      crate::ExecutionFailureKind::Task,
      "first failed later",
      Some(declared_first.id()),
      None,
    );
    tracker
      .complete(
        &first_binding,
        ConsoleStatus::Failed,
        Some(first_failure),
        Default::default(),
      )
      .await
      .unwrap();

    assert_eq!(tracker.failure().await, Some(second_failure));
  }

  #[tokio::test]
  async fn rejects_result_snapshot_before_lifecycle_finishes() {
    let (console, _) = recording_console();
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let binding = ExecutionBinding::for_task(scope.clone());
    let tracker = ExecutionRecorder::new(console, 3, vec![scope], [binding]);

    tracker.declare().await.unwrap();

    assert!(tracker.results().await.is_err());
  }

  #[tokio::test]
  async fn publishes_step_lifecycle_inside_its_parent_task() {
    let (console, renderer) = recording_console();
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let step = allocator.step(&scope, "shell");
    let binding = ExecutionBinding::for_step(scope.clone(), step.clone());
    let tracker = ExecutionRecorder::new(console, 7, vec![scope.clone()], [binding.clone()]);

    tracker.declare().await.unwrap();
    tracker.start_scope(&binding).await.unwrap();
    tracker.start_step(&binding).await.unwrap();
    tracker
      .complete(&binding, ConsoleStatus::Success, None, Default::default())
      .await
      .unwrap();

    assert_eq!(
      *renderer.0.lock().unwrap(),
      [
        ConsoleRecord::Execution(ExecutionEvent::ScopeDeclared {
          run_id: 7,
          scope: scope.clone(),
        }),
        ConsoleRecord::Execution(ExecutionEvent::StepDeclared {
          run_id: 7,
          scope: scope.clone(),
          step: step.clone(),
        }),
        ConsoleRecord::Execution(ExecutionEvent::ScopeStarted {
          run_id: 7,
          scope: scope.clone(),
        }),
        ConsoleRecord::Execution(ExecutionEvent::StepStarted {
          run_id: 7,
          scope: scope.clone(),
          step: step.clone(),
        }),
        ConsoleRecord::Execution(ExecutionEvent::StepFinished {
          run_id: 7,
          scope: scope.clone(),
          step,
          status: ConsoleStatus::Success,
        }),
        ConsoleRecord::Execution(ExecutionEvent::ScopeFinished {
          run_id: 7,
          scope,
          status: ConsoleStatus::Success,
        }),
      ]
    );
  }

  #[tokio::test]
  async fn validates_step_lifecycle_and_finishes_steps_cancelled_before_start() {
    let (console, renderer) = recording_console();
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let step = allocator.step(&scope, "shell");
    let unknown = allocator.step(&scope, "shell");
    let binding = ExecutionBinding::for_step(scope.clone(), step.clone());
    let unknown_binding = ExecutionBinding::for_step(scope.clone(), unknown.clone());
    let tracker = ExecutionRecorder::new(console, 7, vec![scope.clone()], [binding.clone()]);

    assert!(tracker.start_scope(&binding).await.is_err());
    assert!(tracker.start_step(&binding).await.is_err());
    tracker.declare().await.unwrap();
    tracker.start_scope(&binding).await.unwrap();
    assert!(tracker.start_step(&unknown_binding).await.is_err());
    tracker.finish_remaining(ConsoleStatus::Cancelled).await.unwrap();
    assert!(tracker.start_step(&binding).await.is_err());

    assert!(renderer
      .0
      .lock()
      .unwrap()
      .contains(&ConsoleRecord::Execution(ExecutionEvent::StepFinished {
        run_id: 7,
        scope,
        step,
        status: ConsoleStatus::Cancelled,
      })));
  }

  #[tokio::test]
  async fn retries_a_rejected_step_finish() {
    let records = Arc::new(StdMutex::new(Vec::new()));
    let console = Arc::new(Console::new(RejectFirstStepFinish {
      records: records.clone(),
      reject: Arc::new(AtomicBool::new(true)),
    }));
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let step = allocator.step(&scope, "shell");
    let binding = ExecutionBinding::for_step(scope.clone(), step.clone());
    let tracker = ExecutionRecorder::new(console, 7, vec![scope.clone()], [binding.clone()]);

    tracker.declare().await.unwrap();
    tracker.start_scope(&binding).await.unwrap();
    tracker.start_step(&binding).await.unwrap();
    assert!(tracker
      .complete(&binding, ConsoleStatus::Success, None, Default::default())
      .await
      .is_err());
    tracker.finish_remaining(ConsoleStatus::Failed).await.unwrap();

    assert_eq!(
      records
        .lock()
        .unwrap()
        .iter()
        .filter(|record| matches!(record, ConsoleRecord::Execution(ExecutionEvent::StepFinished { .. })))
        .count(),
      2
    );
  }

  #[tokio::test]
  async fn validates_lifecycle_and_aggregates_node_statuses() {
    let (console, renderer) = recording_console();
    let allocator = octa_output::ConsoleScopeAllocator::default();
    let scope = allocator.scope("build");
    let unknown = allocator.scope("unknown");
    let binding = ExecutionBinding::for_task(scope.clone());
    let tracker = ExecutionRecorder::new(console, 8, Vec::new(), [binding.clone(), binding.clone()]);

    let unknown_binding = ExecutionBinding::for_task(unknown);
    assert!(tracker
      .complete(&binding, ConsoleStatus::Success, None, Default::default())
      .await
      .is_err());
    assert!(tracker
      .complete(&unknown_binding, ConsoleStatus::Success, None, Default::default(),)
      .await
      .is_err());

    tracker.declare().await.unwrap();
    tracker.start_scope(&binding).await.unwrap();
    tracker
      .complete(&binding, ConsoleStatus::Success, None, Default::default())
      .await
      .unwrap();
    tracker
      .complete(&binding, ConsoleStatus::Skipped, None, Default::default())
      .await
      .unwrap();

    assert_eq!(tracker.successful_run_status().await, ConsoleStatus::Success);
    assert!(tracker
      .complete(&binding, ConsoleStatus::Success, None, Default::default())
      .await
      .is_err());
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
    let first_binding = ExecutionBinding::for_task(first.clone());
    let second_binding = ExecutionBinding::for_task(second.clone());
    let tracker = ExecutionRecorder::new(
      console,
      9,
      vec![first.clone(), second.clone()],
      [first_binding.clone(), second_binding.clone()],
    );

    tracker.declare().await.unwrap();
    tracker.start_scope(&first_binding).await.unwrap();
    tracker.start_scope(&second_binding).await.unwrap();
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

  #[test]
  fn originating_failure_replaces_secondary_cancellation() {
    let mut recorded = Some(ExecutionFailure::synthetic(
      crate::ExecutionFailureKind::Cancelled,
      "cancelled",
      Some(1),
      None,
    ));
    let failure = ExecutionFailure::synthetic(crate::ExecutionFailureKind::Task, "failed", Some(1), None);

    record_failure(&mut recorded, Some(failure.clone()));
    record_failure(
      &mut recorded,
      Some(ExecutionFailure::synthetic(
        crate::ExecutionFailureKind::Cancelled,
        "later cancellation",
        Some(1),
        None,
      )),
    );
    record_failure(&mut recorded, None);

    assert_eq!(recorded, Some(failure));
  }
}
