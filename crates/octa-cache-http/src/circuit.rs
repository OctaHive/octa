//! Small per-client circuit breaker that prevents failure amplification.

use std::{sync::Mutex, time::Instant};

use crate::HttpCacheError;

#[derive(Debug, Default)]
struct State {
  generation: u64,
  failures: u32,
  open_until: Option<Instant>,
}

/// Generation observed when one operation entered the circuit.
///
/// A late success from an older generation must not close a circuit opened by
/// another in-flight operation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CircuitPermit(u64);

#[derive(Debug)]
pub(crate) struct CircuitBreaker {
  threshold: u32,
  open_for: std::time::Duration,
  state: Mutex<State>,
}

impl CircuitBreaker {
  pub(crate) fn new(threshold: u32, open_for: std::time::Duration) -> Self {
    Self {
      threshold,
      open_for,
      state: Mutex::new(State::default()),
    }
  }

  pub(crate) fn permit(&self) -> Result<CircuitPermit, HttpCacheError> {
    let mut state = self.state.lock().expect("cache circuit mutex poisoned");
    if state.open_until.is_some_and(|deadline| deadline > Instant::now()) {
      return Err(HttpCacheError::CircuitOpen);
    }
    if state.open_until.take().is_some() {
      state.failures = 0;
      state.generation = state.generation.wrapping_add(1);
    }
    Ok(CircuitPermit(state.generation))
  }

  pub(crate) fn success(&self, permit: CircuitPermit) {
    let mut state = self.state.lock().expect("cache circuit mutex poisoned");
    if state.generation == permit.0 && state.open_until.is_none() {
      state.failures = 0;
    }
  }

  pub(crate) fn failure(&self, permit: CircuitPermit) {
    let mut state = self.state.lock().expect("cache circuit mutex poisoned");
    if state.generation != permit.0 {
      return;
    }
    state.failures = state.failures.saturating_add(1);
    if state.failures >= self.threshold {
      state.open_until = Instant::now().checked_add(self.open_for);
      state.generation = state.generation.wrapping_add(1);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_late_success_cannot_close_a_newer_open_circuit() {
    let circuit = CircuitBreaker::new(1, std::time::Duration::from_secs(60));
    let late = circuit.permit().unwrap();
    let failing = circuit.permit().unwrap();
    circuit.failure(failing);
    circuit.success(late);
    assert!(matches!(circuit.permit(), Err(HttpCacheError::CircuitOpen)));
  }

  #[test]
  fn an_expired_circuit_starts_a_fresh_generation() {
    let circuit = CircuitBreaker::new(1, std::time::Duration::ZERO);
    let expired = circuit.permit().unwrap();
    circuit.failure(expired);
    let fresh = circuit.permit().unwrap();
    circuit.failure(expired);
    circuit.success(fresh);
    assert!(circuit.permit().is_ok());
  }
}
