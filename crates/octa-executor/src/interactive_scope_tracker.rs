//! Runtime isolation across the nodes of one interactive task invocation.
//!
//! Interactive task bodies expand into several DAG nodes but must hold one
//! exclusive runtime guard from the first node until the last node completes.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;

use crate::runtime_coordinator::{RuntimeCoordinator, RuntimeGuard};

struct InteractiveState {
  remaining: usize,
  guard: Option<RuntimeGuard>,
}

/// Holds a task-runtime write lock from the first body node until the last one finishes.
pub(crate) struct InteractiveScopeTracker {
  coordinator: Arc<RuntimeCoordinator>,
  states: HashMap<String, Mutex<InteractiveState>>,
}

impl InteractiveScopeTracker {
  pub(crate) fn new(
    coordinator: Arc<RuntimeCoordinator>,
    nodes: impl IntoIterator<Item = (Option<String>, bool)>,
  ) -> Self {
    let mut counts = HashMap::new();
    for (session, needs_lock) in nodes {
      if needs_lock {
        if let Some(session) = session {
          *counts.entry(session).or_insert(0) += 1;
        }
      }
    }
    let states = counts
      .into_iter()
      .map(|(session, remaining)| (session, Mutex::new(InteractiveState { remaining, guard: None })))
      .collect();
    Self { coordinator, states }
  }

  pub(crate) async fn enter(&self, session: Option<&str>, needs_lock: bool) -> Option<RuntimeGuard> {
    if !needs_lock {
      return None;
    }
    let Some(session) = session else {
      return Some(self.coordinator.guard(false).await);
    };
    let state = self
      .states
      .get(session)
      .expect("interactive session was collected from the same execution plan");
    let mut state = state.lock().await;
    if state.guard.is_none() {
      state.guard = Some(self.coordinator.guard(true).await);
    }
    None
  }

  pub(crate) async fn complete(&self, session: Option<&str>, needs_lock: bool) {
    if !needs_lock {
      return;
    }
    let Some(session) = session else {
      return;
    };
    let Some(state) = self.states.get(session) else {
      return;
    };
    let mut state = state.lock().await;
    state.remaining = state.remaining.saturating_sub(1);
    if state.remaining == 0 {
      state.guard.take();
    }
  }

  pub(crate) async fn finish_remaining(&self) {
    for state in self.states.values() {
      state.lock().await.guard.take();
    }
  }
}

#[cfg(test)]
mod tests {
  use std::time::Duration;

  use tokio::time::timeout;

  use super::*;

  #[tokio::test]
  async fn interactive_session_holds_exclusive_lock_until_every_node_completes() {
    let coordinator = Arc::new(RuntimeCoordinator::default());
    let tracker = InteractiveScopeTracker::new(
      coordinator.clone(),
      [(Some("session".to_owned()), true), (Some("session".to_owned()), true)],
    );

    tracker.enter(Some("session"), true).await;
    assert!(timeout(Duration::from_millis(20), coordinator.guard(false))
      .await
      .is_err());

    tracker.complete(Some("session"), true).await;
    assert!(timeout(Duration::from_millis(20), coordinator.guard(false))
      .await
      .is_err());

    tracker.complete(Some("session"), true).await;
    assert!(timeout(Duration::from_millis(100), coordinator.guard(false))
      .await
      .is_ok());
  }

  #[tokio::test]
  async fn regular_nodes_share_the_runtime_lock() {
    let coordinator = Arc::new(RuntimeCoordinator::default());
    let tracker = InteractiveScopeTracker::new(coordinator.clone(), [(None, true)]);

    let shared = tracker.enter(None, true).await;
    assert!(timeout(Duration::from_millis(100), coordinator.guard(false))
      .await
      .is_ok());
    assert!(timeout(Duration::from_millis(20), coordinator.guard(true))
      .await
      .is_err());
    drop(shared);
    assert!(timeout(Duration::from_millis(100), coordinator.guard(true))
      .await
      .is_ok());
  }
}
