use std::sync::Arc;

use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

/// Coordinates execution isolation across independently built task plans.
#[derive(Debug, Clone, Default)]
pub struct RuntimeCoordinator {
  task_lock: Arc<RwLock<()>>,
}

pub(crate) enum RuntimeGuard {
  Shared { _guard: OwnedRwLockReadGuard<()> },
  Exclusive { _guard: OwnedRwLockWriteGuard<()> },
}

impl RuntimeCoordinator {
  pub(crate) async fn guard(&self, exclusive: bool) -> RuntimeGuard {
    if exclusive {
      RuntimeGuard::Exclusive {
        _guard: self.task_lock.clone().write_owned().await,
      }
    } else {
      RuntimeGuard::Shared {
        _guard: self.task_lock.clone().read_owned().await,
      }
    }
  }
}
