//! Fail-closed registration of finalized Codex resources.
//!
//! The harness process tree is already gone when this module runs. It opens
//! every run record and exact user deliverable relative to one capability
//! directory, retains the opened filesystem identities, and then reopens the
//! complete set before emitting any protocol response. This catches missing,
//! replaced, linked, and special-file leaves without reimplementing Octa's
//! recursive artifact collection. Octa remains the final containment and
//! link-safety authority when it consumes the standard declarations.

use std::{
  collections::HashSet,
  fs::Metadata,
  path::{Path, PathBuf},
  sync::Arc,
  time::SystemTime,
};

use anyhow::{bail, Context};
use cap_std::{
  ambient_authority,
  fs::{Dir, OpenOptions},
};
use octa_plugin::{
  protocol::{ArtifactDeclaration, PluginResponse, ReportDeclaration},
  send_response,
};
use tokio::{io::AsyncWrite, sync::Mutex};

use crate::{
  config::Deliverable,
  contract::{result_report_format, PROVENANCE_ARTIFACT_NAME, RESULT_REPORT_NAME, TRACE_ARTIFACT_NAME},
  filesystem::{is_std_link_or_reparse, open_directory_no_follow},
  records::RunRecordPaths,
};

const JSON_CONTENT_TYPE: &str = "application/json";
const JSONL_CONTENT_TYPE: &str = "application/x-ndjson";

/// Validates the complete resource set and emits only existing protocol DTOs.
///
/// Validation finishes before the first response is written. The retained
/// file handles remain alive through emission so an identity cannot be reused
/// unnoticed on filesystems where identity uniqueness depends on open handles.
pub(crate) async fn publish<W>(
  writer: &Arc<Mutex<W>>,
  command_id: &str,
  working_directory: &Path,
  records: &RunRecordPaths,
  deliverables: &[Deliverable],
) -> anyhow::Result<()>
where
  W: AsyncWrite + Send + 'static + Unpin,
{
  let working_directory = working_directory.to_owned();
  let records = records.clone();
  let deliverables = deliverables.to_vec();
  let validated = tokio::task::spawn_blocking(move || {
    let resources = ResourceSet::inspect(&working_directory, &records, &deliverables)?;
    resources.ensure_unchanged()?;
    Ok::<_, anyhow::Error>(resources)
  })
  .await
  .context("Codex resource validation task failed")??;

  for declaration in &validated.declarations {
    let response = declaration.response(command_id);
    send_response(writer, &response).await?;
  }
  Ok(())
}

pub(super) struct ResourceSet {
  root: Dir,
  declarations: Vec<ResourceDeclaration>,
  snapshots: Vec<ResourceSnapshot>,
}

#[derive(Clone)]
enum ResourceDeclaration {
  Artifact(ArtifactDeclaration),
  Report(ReportDeclaration),
}

struct ResourceSnapshot {
  path: String,
  expected: ExpectedKind,
  opened: OpenedResource,
}

#[derive(Clone, Copy)]
pub(super) enum ExpectedKind {
  File,
  FileOrDirectory,
}

pub(super) struct OpenedResource {
  identity: same_file::Handle,
  kind: ResourceKind,
  length: u64,
  modified: Option<SystemTime>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ResourceKind {
  File,
  Directory,
}

impl ResourceSet {
  pub(super) fn inspect(
    working_directory: &Path,
    records: &RunRecordPaths,
    deliverables: &[Deliverable],
  ) -> anyhow::Result<Self> {
    let root = Dir::open_ambient_dir(working_directory, ambient_authority())
      .context("failed to open the effective task directory for Codex resources")?;
    let declarations = declarations(records, deliverables)?;
    let snapshots = declarations
      .iter()
      .map(|declaration| {
        let (path, expected) = declaration.path_and_kind();
        let opened = open_resource(&root, path, expected)?;
        Ok(ResourceSnapshot {
          path: path.to_owned(),
          expected,
          opened,
        })
      })
      .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(Self {
      root,
      declarations,
      snapshots,
    })
  }

  pub(super) fn ensure_unchanged(&self) -> anyhow::Result<()> {
    for snapshot in &self.snapshots {
      let current = open_resource(&self.root, &snapshot.path, snapshot.expected)?;
      if !snapshot.opened.same_generation(&current) {
        bail!("Codex resource '{}' changed during final validation", snapshot.path);
      }
    }
    Ok(())
  }

  #[cfg(test)]
  pub(super) fn responses(&self, command_id: &str) -> Vec<PluginResponse> {
    self
      .declarations
      .iter()
      .map(|declaration| declaration.response(command_id))
      .collect()
  }
}

impl ResourceDeclaration {
  fn path_and_kind(&self) -> (&str, ExpectedKind) {
    match self {
      Self::Artifact(artifact) => (
        artifact
          .path
          .to_str()
          .expect("declarations originate from UTF-8 strings"),
        ExpectedKind::FileOrDirectory,
      ),
      Self::Report(report) => (
        report.path.to_str().expect("declarations originate from UTF-8 strings"),
        ExpectedKind::File,
      ),
    }
  }

  fn response(&self, command_id: &str) -> PluginResponse {
    match self {
      Self::Artifact(artifact) => PluginResponse::RegisterArtifact {
        id: command_id.to_owned(),
        artifact: artifact.clone(),
      },
      Self::Report(report) => PluginResponse::RegisterReport {
        id: command_id.to_owned(),
        report: report.clone(),
      },
    }
  }
}

impl OpenedResource {
  pub(super) fn from_file(file: std::fs::File, kind: ResourceKind) -> anyhow::Result<Self> {
    let metadata = file.metadata().context("failed to inspect an opened Codex resource")?;
    if !kind.matches(&metadata) || is_std_link_or_reparse(&metadata) {
      bail!("Codex resource changed type while it was opened");
    }
    let length = metadata.len();
    let modified = metadata.modified().ok();
    let identity = same_file::Handle::from_file(file).context("failed to identify an opened Codex resource")?;
    Ok(Self {
      identity,
      kind,
      length,
      modified,
    })
  }

  fn same_generation(&self, current: &Self) -> bool {
    self.identity == current.identity
      && self.kind == current.kind
      && (self.kind == ResourceKind::Directory || (self.length == current.length && self.modified == current.modified))
  }
}

impl ResourceKind {
  fn matches(self, metadata: &Metadata) -> bool {
    match self {
      Self::File => metadata.is_file(),
      Self::Directory => metadata.is_dir(),
    }
  }
}

fn declarations(records: &RunRecordPaths, deliverables: &[Deliverable]) -> anyhow::Result<Vec<ResourceDeclaration>> {
  let mut declarations = vec![
    ResourceDeclaration::Artifact(ArtifactDeclaration {
      name: TRACE_ARTIFACT_NAME.to_owned(),
      path: PathBuf::from(&records.trace),
      content_type: Some(JSONL_CONTENT_TYPE.to_owned()),
    }),
    ResourceDeclaration::Artifact(ArtifactDeclaration {
      name: PROVENANCE_ARTIFACT_NAME.to_owned(),
      path: PathBuf::from(&records.provenance),
      content_type: Some(JSON_CONTENT_TYPE.to_owned()),
    }),
    ResourceDeclaration::Report(ReportDeclaration {
      name: RESULT_REPORT_NAME.to_owned(),
      path: PathBuf::from(&records.result),
      format: result_report_format(),
    }),
  ];

  for deliverable in deliverables {
    declarations.push(match deliverable {
      Deliverable::Artifact {
        name,
        path,
        content_type,
      } => ResourceDeclaration::Artifact(ArtifactDeclaration {
        name: name.clone(),
        path: PathBuf::from(path.as_str()),
        content_type: content_type.clone(),
      }),
      Deliverable::Report { name, path, format } => ResourceDeclaration::Report(ReportDeclaration {
        name: name.clone(),
        path: PathBuf::from(path.as_str()),
        format: format.clone(),
      }),
    });
  }

  let mut names = HashSet::with_capacity(declarations.len());
  for declaration in &declarations {
    let name = match declaration {
      ResourceDeclaration::Artifact(artifact) => &artifact.name,
      ResourceDeclaration::Report(report) => &report.name,
    };
    if !names.insert(name.as_str()) {
      bail!("Codex resource name '{name}' is configured more than once");
    }
  }
  Ok(declarations)
}

pub(super) fn open_resource(root: &Dir, path: &str, expected: ExpectedKind) -> anyhow::Result<OpenedResource> {
  if path.is_empty() {
    bail!("required Codex resource path must not be empty");
  }
  let mut directory = root.try_clone().context("failed to retain the Codex resource root")?;
  let mut components = path.split('/').peekable();
  while let Some(component) = components.next() {
    let metadata = directory
      .symlink_metadata(component)
      .with_context(|| format!("required Codex resource '{path}' is missing or inaccessible"))?;
    if is_cap_link_or_reparse(&metadata) {
      bail!("required Codex resource '{path}' contains a symbolic link or reparse point");
    }

    if components.peek().is_some() {
      if !metadata.is_dir() {
        bail!("required Codex resource '{path}' has a non-directory ancestor");
      }
      directory = open_directory(&directory, component, path)?;
      continue;
    }

    if metadata.is_file() {
      let mut options = OpenOptions::new();
      options.read(true);
      configure_resource_open(&mut options);
      let file = directory
        .open_with(component, &options)
        .with_context(|| format!("required Codex resource '{path}' could not be opened safely"))?
        .into_std();
      return OpenedResource::from_file(file, ResourceKind::File);
    }
    if metadata.is_dir() && matches!(expected, ExpectedKind::FileOrDirectory) {
      let directory = open_directory(&directory, component, path)?;
      return OpenedResource::from_file(directory.into_std_file(), ResourceKind::Directory);
    }
    if metadata.is_dir() {
      bail!("required Codex report '{path}' must be a regular file");
    }
    bail!("required Codex resource '{path}' must be a regular file or directory");
  }
  unreachable!("a non-empty split always visits a final resource component")
}

pub(super) fn open_directory(parent: &Dir, component: &str, complete_path: &str) -> anyhow::Result<Dir> {
  open_directory_no_follow(parent, component)
    .with_context(|| format!("required Codex resource '{complete_path}' could not be opened safely"))
}

fn is_cap_link_or_reparse(metadata: &cap_std::fs::Metadata) -> bool {
  if metadata.is_symlink() {
    return true;
  }
  #[cfg(windows)]
  {
    use cap_std::fs::MetadataExt as _;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
  }
  #[cfg(not(windows))]
  false
}

#[cfg(unix)]
fn configure_resource_open(options: &mut OpenOptions) {
  use cap_std::fs::OpenOptionsExt as _;

  options.custom_flags(libc::O_NOFOLLOW);
}

#[cfg(windows)]
fn configure_resource_open(options: &mut OpenOptions) {
  use cap_std::fs::OpenOptionsExt as _;
  use windows_sys::Win32::Storage::FileSystem::{FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT};

  options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
}

#[cfg(not(any(unix, windows)))]
fn configure_resource_open(_options: &mut OpenOptions) {}
