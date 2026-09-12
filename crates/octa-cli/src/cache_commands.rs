//! Cache-management commands at the CLI composition boundary.
//!
//! Status and pruning need only the operational cache profile. Explain uses a
//! fully loaded runtime because an exact action identity includes resolved
//! variables, secrets, plugin evaluations, and the input snapshot. It never
//! executes task bodies, but those dynamic context providers can have their
//! own side effects; the command documents that distinction explicitly.

use std::{path::Path, sync::Arc};

use octa_output::{Console, ConsoleLevel};
use octa_runtime::{RunOptions, Runtime, RuntimeCacheConfig};

use super::{CacheCommand, Cli, OctaError, OctaResult};

pub(super) fn explain_task(command: Option<&CacheCommand>) -> Option<String> {
  match command {
    Some(CacheCommand::Explain { task }) => Some(task.clone()),
    _ => None,
  }
}

pub(super) async fn run_management(
  command: &CacheCommand,
  workspace: &Path,
  cache_profile: Option<&Path>,
  console: &Console,
) -> OctaResult<()> {
  let profile = cache_profile.ok_or(OctaError::CacheProfileRequired)?;
  let cache = RuntimeCacheConfig::load_profile(profile, workspace)?.open().await?;
  match command {
    CacheCommand::Status => {
      let status = cache.status().await?;
      console
        .message(
          ConsoleLevel::Info,
          format!(
            "Cache {}: {} of {} bytes used (collect at {}, target {})",
            status.layout.display(),
            status.used_bytes,
            status.max_bytes,
            status.high_watermark_bytes,
            status.low_watermark_bytes
          ),
        )
        .await?;
    },
    CacheCommand::Prune => {
      let report = cache.prune().await?;
      console
        .message(
          ConsoleLevel::Info,
          format!(
            "Cache pruned: {} -> {} bytes, {} actions, {} blobs, {} maintenance files removed",
            report.bytes_before,
            report.bytes_after,
            report.actions_removed,
            report.blobs_removed,
            report.maintenance_files_removed
          ),
        )
        .await?;
    },
    CacheCommand::Explain { .. } => return Err(OctaError::CacheExplainUnavailable),
  }
  Ok(())
}

pub(super) async fn run_explain(runtime: Arc<Runtime>, console: &Console, task: String, args: &Cli) -> OctaResult<()> {
  let commands = runtime.qualify_commands(vec![task]);
  let options = RunOptions {
    variables: args.vars.clone(),
    task_args: args.task_args.clone(),
    quiet: true,
    cache_probe: true,
    ..RunOptions::default()
  };
  let (prepared, _) = runtime.prepare(&commands, &options).await?;
  let results = runtime.execute(prepared, false, false).await?;
  let mut explained = 0;
  for result in results {
    for task in result.tasks {
      let Some(cache) = task.cache else {
        continue;
      };
      explained += 1;
      let action = cache.action.as_deref().unwrap_or("unavailable");
      let reason = cache.reason.map(|reason| reason.as_str()).unwrap_or("none");
      let layer = match cache.layer {
        Some(octa_cache_protocol::CacheLayer::Local) => "local",
        Some(octa_cache_protocol::CacheLayer::Remote) => "remote",
        None => "none",
        Some(_) => "unknown",
      };
      console
        .message(
          ConsoleLevel::Info,
          format!(
            "{}: status={}, action={}, layer={}, reason={}",
            task.label,
            cache_status_name(cache.status),
            action,
            layer,
            reason
          ),
        )
        .await?;
    }
  }
  if explained == 0 {
    return Err(OctaError::CacheExplainUnavailable);
  }
  Ok(())
}

fn cache_status_name(status: octa_executor::CacheStatus) -> &'static str {
  match status {
    octa_executor::CacheStatus::Hit => "hit",
    octa_executor::CacheStatus::Miss => "miss",
    octa_executor::CacheStatus::Bypassed => "bypassed",
    octa_executor::CacheStatus::Error => "error",
    _ => "unknown",
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cache_statuses_have_stable_human_readable_names() {
    assert_eq!(cache_status_name(octa_executor::CacheStatus::Hit), "hit");
    assert_eq!(cache_status_name(octa_executor::CacheStatus::Miss), "miss");
    assert_eq!(cache_status_name(octa_executor::CacheStatus::Bypassed), "bypassed");
    assert_eq!(cache_status_name(octa_executor::CacheStatus::Error), "error");
  }
}
