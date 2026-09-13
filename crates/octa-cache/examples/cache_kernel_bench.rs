//! Release-mode throughput probe for the public cache streaming primitives.
//!
//! The end-to-end Python suite owns fixture generation and process resource
//! measurement. This executable reports only durations observed immediately
//! around the real snapshot, pack, and extract APIs, avoiding CLI/plugin
//! startup noise in the fixed throughput thresholds.

use std::{env, fs, path::PathBuf, time::Instant};

use octa_cache::{extract_bundle, pack_bundle, BundleEncoding, BundleLimits, InputSnapshotter, SnapshotOptions};
use octa_cache_protocol::RelativePath;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
  let mut arguments = env::args_os().skip(1);
  let operation = arguments.next().ok_or("missing operation")?;
  let operation = operation.to_str().ok_or("operation must be UTF-8")?;
  let workspace = absolute(arguments.next(), "workspace")?;
  let output = arguments.next().ok_or("missing input pattern or output root")?;
  let output = output.to_str().ok_or("pattern or output root must be UTF-8")?;
  let cancel = CancellationToken::new();

  match operation {
    "snapshot" => {
      ensure_finished(arguments)?;
      let snapshotter = InputSnapshotter::new(SnapshotOptions::default())?;
      let started = Instant::now();
      let snapshot = snapshotter.snapshot(&workspace, &[output.to_owned()], &cancel).await?;
      println!(
        "{}",
        json!({
          "operation": "snapshot",
          "elapsed_ns": started.elapsed().as_nanos(),
          "entries": snapshot.entries.len(),
          "digest": snapshot.root,
        })
      );
    },
    "roundtrip" => {
      let bundle = absolute(arguments.next(), "bundle")?;
      let staging = absolute(arguments.next(), "staging")?;
      ensure_finished(arguments)?;
      let output = RelativePath::new(output)?;
      let limits = BundleLimits::default();
      let file = fs::File::create(&bundle)?;
      let pack_started = Instant::now();
      let packed = pack_bundle(
        file,
        &workspace,
        std::slice::from_ref(&output),
        BundleEncoding::default(),
        limits,
        &cancel,
      )?;
      packed.writer.sync_all()?;
      let pack_elapsed = pack_started.elapsed();
      fs::create_dir_all(&staging)?;
      let extract_started = Instant::now();
      extract_bundle(
        fs::File::open(&bundle)?,
        &packed.descriptor,
        &staging,
        std::slice::from_ref(&output),
        limits,
        &cancel,
      )?;
      let extract_elapsed = extract_started.elapsed();
      println!(
        "{}",
        json!({
          "operation": "roundtrip",
          "pack_elapsed_ns": pack_elapsed.as_nanos(),
          "extract_elapsed_ns": extract_elapsed.as_nanos(),
          "descriptor": packed.descriptor,
        })
      );
    },
    _ => return Err(format!("unknown operation '{operation}'").into()),
  }
  Ok(())
}

fn absolute(value: Option<std::ffi::OsString>, name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
  let path = PathBuf::from(value.ok_or_else(|| format!("missing {name}"))?);
  if !path.is_absolute() {
    return Err(format!("{name} must be absolute").into());
  }
  Ok(path)
}

fn ensure_finished(mut arguments: impl Iterator<Item = std::ffi::OsString>) -> Result<(), Box<dyn std::error::Error>> {
  if arguments.next().is_some() {
    return Err("too many arguments".into());
  }
  Ok(())
}
