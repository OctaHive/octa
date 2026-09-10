use std::path::Path;

use octa_octafile::{ArtifactDeclaration as OctafileArtifact, ReportDeclaration as OctafileReport};
use octa_output::{RegisteredArtifact, RegisteredReport};
use octa_plugin::protocol::{ArtifactDeclaration as PluginArtifact, ReportDeclaration as PluginReport};

use crate::error::{ExecutorError, ExecutorResult};

pub(crate) fn octafile_artifacts(
  declarations: &[OctafileArtifact],
  working_dir: &Path,
  workspace: &Path,
) -> ExecutorResult<Vec<RegisteredArtifact>> {
  declarations
    .iter()
    .map(|declaration| {
      Ok(RegisteredArtifact {
        name: validate_name("artifact", &declaration.name)?,
        path: normalize_path(
          "artifact",
          &declaration.name,
          &declaration.path,
          working_dir,
          workspace,
          false,
        )?,
        content_type: declaration.content_type.clone(),
      })
    })
    .collect()
}

pub(crate) fn octafile_reports(
  declarations: &[OctafileReport],
  working_dir: &Path,
  workspace: &Path,
) -> ExecutorResult<Vec<RegisteredReport>> {
  declarations
    .iter()
    .map(|declaration| {
      Ok(RegisteredReport {
        name: validate_name("report", &declaration.name)?,
        path: normalize_path(
          "report",
          &declaration.name,
          &declaration.path,
          working_dir,
          workspace,
          true,
        )?,
        format: validate_format(&declaration.name, &declaration.format)?,
      })
    })
    .collect()
}

pub(crate) fn plugin_artifacts(
  declarations: &[PluginArtifact],
  working_dir: &Path,
  workspace: &Path,
) -> ExecutorResult<Vec<RegisteredArtifact>> {
  declarations
    .iter()
    .map(|declaration| {
      Ok(RegisteredArtifact {
        name: validate_name("artifact", &declaration.name)?,
        path: normalize_path(
          "artifact",
          &declaration.name,
          &declaration.path,
          working_dir,
          workspace,
          false,
        )?,
        content_type: declaration.content_type.clone(),
      })
    })
    .collect()
}

pub(crate) fn plugin_reports(
  declarations: &[PluginReport],
  working_dir: &Path,
  workspace: &Path,
) -> ExecutorResult<Vec<RegisteredReport>> {
  declarations
    .iter()
    .map(|declaration| {
      Ok(RegisteredReport {
        name: validate_name("report", &declaration.name)?,
        path: normalize_path(
          "report",
          &declaration.name,
          &declaration.path,
          working_dir,
          workspace,
          true,
        )?,
        format: validate_format(&declaration.name, &declaration.format)?,
      })
    })
    .collect()
}

fn validate_name(kind: &'static str, name: &str) -> ExecutorResult<String> {
  if name.trim().is_empty() {
    return Err(ExecutorError::InvalidResource {
      kind,
      name: name.to_owned(),
      message: "name cannot be empty".to_owned(),
    });
  }
  Ok(name.to_owned())
}

fn validate_format(name: &str, format: &str) -> ExecutorResult<String> {
  let mut chars = format.chars();
  let valid = format.len() <= 128
    && chars.next().is_some_and(|character| character.is_ascii_alphanumeric())
    && chars
      .all(|character| character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '/' | '+' | ':' | '-'));
  if !valid {
    return Err(ExecutorError::InvalidResource {
      kind: "report",
      name: name.to_owned(),
      message: "format must be a 1-128 character identifier".to_owned(),
    });
  }
  Ok(format.to_owned())
}

fn normalize_path(
  kind: &'static str,
  name: &str,
  path: &Path,
  working_dir: &Path,
  workspace: &Path,
  require_file: bool,
) -> ExecutorResult<String> {
  if path.is_absolute() {
    return Err(invalid_path(
      kind,
      name,
      path,
      "path must be relative to the task working directory",
    ));
  }

  let workspace = dunce::canonicalize(workspace)
    .map_err(|error| invalid_path(kind, name, path, format!("cannot resolve workspace: {error}")))?;
  let resolved = dunce::canonicalize(working_dir.join(path))
    .map_err(|error| invalid_path(kind, name, path, format!("cannot resolve path: {error}")))?;
  let relative = resolved
    .strip_prefix(&workspace)
    .map_err(|_| invalid_path(kind, name, path, "resolved path is outside the workspace"))?;

  if relative.as_os_str().is_empty() {
    return Err(invalid_path(kind, name, path, "workspace root cannot be registered"));
  }
  if require_file && !resolved.is_file() {
    return Err(invalid_path(kind, name, path, "report path must resolve to a file"));
  }

  relative
    .iter()
    .map(|component| {
      component
        .to_str()
        .ok_or_else(|| invalid_path(kind, name, path, "path must contain valid UTF-8"))
    })
    .collect::<ExecutorResult<Vec<_>>>()
    .map(|components| components.join("/"))
}

fn invalid_path(kind: &'static str, name: &str, path: &Path, message: impl Into<String>) -> ExecutorError {
  ExecutorError::InvalidResource {
    kind,
    name: name.to_owned(),
    message: format!("{} ({message})", path.display(), message = message.into()),
  }
}

#[cfg(test)]
mod tests {
  use std::fs;
  use std::path::PathBuf;

  use super::*;

  #[test]
  fn normalizes_paths_relative_to_workspace() {
    let workspace = tempfile::tempdir().unwrap();
    let working_dir = workspace.path().join("project");
    fs::create_dir(&working_dir).unwrap();
    fs::write(working_dir.join("junit.xml"), "<testsuite/>").unwrap();
    let reports = octafile_reports(
      &[OctafileReport {
        name: "tests".to_owned(),
        path: PathBuf::from("junit.xml"),
        format: "junit".to_owned(),
      }],
      &working_dir,
      workspace.path(),
    )
    .unwrap();

    assert_eq!(reports[0].path, "project/junit.xml");
  }

  #[cfg(unix)]
  #[test]
  fn rejects_symlinks_that_escape_workspace() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    symlink(outside.path(), workspace.path().join("outside")).unwrap();
    let error = octafile_artifacts(
      &[OctafileArtifact {
        name: "escape".to_owned(),
        path: PathBuf::from("outside"),
        content_type: None,
      }],
      workspace.path(),
      workspace.path(),
    )
    .unwrap_err();

    assert!(error.to_string().contains("outside the workspace"));
  }

  #[test]
  fn accepts_artifact_directories_but_requires_report_files() {
    let workspace = tempfile::tempdir().unwrap();
    fs::create_dir(workspace.path().join("dist")).unwrap();
    let artifacts = octafile_artifacts(
      &[OctafileArtifact {
        name: "bundle".to_owned(),
        path: "dist".into(),
        content_type: Some("application/octet-stream".to_owned()),
      }],
      workspace.path(),
      workspace.path(),
    )
    .unwrap();
    assert_eq!(artifacts[0].path, "dist");

    let error = octafile_reports(
      &[OctafileReport {
        name: "tests".to_owned(),
        path: "dist".into(),
        format: "junit".to_owned(),
      }],
      workspace.path(),
      workspace.path(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("must resolve to a file"));
  }

  #[test]
  fn rejects_empty_names_absolute_paths_workspace_root_and_missing_paths() {
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("result"), "ok").unwrap();
    for (name, path, expected) in [
      (" ", PathBuf::from("result"), "name cannot be empty"),
      ("result", workspace.path().join("result"), "must be relative"),
      ("result", PathBuf::from("."), "workspace root"),
      ("result", PathBuf::from("missing"), "cannot resolve path"),
    ] {
      let error = octafile_artifacts(
        &[OctafileArtifact {
          name: name.to_owned(),
          path,
          content_type: None,
        }],
        workspace.path(),
        workspace.path(),
      )
      .unwrap_err();
      assert!(error.to_string().contains(expected), "unexpected error: {error}");
    }
  }

  #[test]
  fn preserves_plugin_defined_report_formats() {
    let workspace = tempfile::tempdir().unwrap();
    for file in ["junit.xml", "coverage.xml", "results.sarif"] {
      fs::write(workspace.path().join(file), "{}").unwrap();
    }
    let artifacts = plugin_artifacts(
      &[PluginArtifact {
        name: "result".to_owned(),
        path: "junit.xml".into(),
        content_type: None,
      }],
      workspace.path(),
      workspace.path(),
    )
    .unwrap();
    assert_eq!(artifacts[0].path, "junit.xml");

    let reports = plugin_reports(
      &[
        PluginReport {
          name: "junit".to_owned(),
          path: "junit.xml".into(),
          format: "junit".to_owned(),
        },
        PluginReport {
          name: "coverage".to_owned(),
          path: "coverage.xml".into(),
          format: "cobertura".to_owned(),
        },
        PluginReport {
          name: "analysis".to_owned(),
          path: "results.sarif".into(),
          format: "acme/analysis-v2".to_owned(),
        },
      ],
      workspace.path(),
      workspace.path(),
    )
    .unwrap();
    assert_eq!(reports[0].format, "junit");
    assert_eq!(reports[1].format, "cobertura");
    assert_eq!(reports[2].format, "acme/analysis-v2");
  }

  #[test]
  fn rejects_invalid_report_format_identifiers() {
    let workspace = tempfile::tempdir().unwrap();
    fs::write(workspace.path().join("report.xml"), "<report/>").unwrap();
    for format in ["", " junit", "junit report", "@junit"] {
      let error = plugin_reports(
        &[PluginReport {
          name: "tests".to_owned(),
          path: "report.xml".into(),
          format: format.to_owned(),
        }],
        workspace.path(),
        workspace.path(),
      )
      .unwrap_err();
      assert!(error.to_string().contains("format must be"));
    }
  }
}
