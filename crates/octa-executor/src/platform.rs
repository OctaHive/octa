//! Platform selector normalization and filtering for tasks and commands.

use super::{FindResult, TaskGraphBuilder};

pub(super) fn normalize_os(value: &str) -> String {
  match normalize(value).as_str() {
    "darwin" | "osx" => "macos".to_owned(),
    os => os.to_owned(),
  }
}

pub(super) fn normalize_architecture(value: &str) -> String {
  match normalize(value).as_str() {
    "amd64" | "x64" | "x86-64" => "x86_64".to_owned(),
    "aarch64" => "arm64".to_owned(),
    architecture => architecture.to_owned(),
  }
}

fn normalize(value: &str) -> String {
  value
    .chars()
    .filter(|character| !character.is_whitespace())
    .flat_map(char::to_lowercase)
    .collect()
}

pub(super) fn matches(selector: &str, os_type: &str, os_arch: &str) -> bool {
  if let Some((platform, architecture)) = selector.split_once('/') {
    return normalize_os(platform) == os_type && normalize_architecture(architecture) == os_arch;
  }

  normalize_os(selector) == os_type || normalize_architecture(selector) == os_arch
}

impl TaskGraphBuilder {
  pub(super) fn filter_command_by_platform(&self, commands: Vec<FindResult>) -> Vec<FindResult> {
    commands
      .into_iter()
      .filter(|command| self.matches_platforms(command.task.platforms.as_deref()))
      .collect()
  }

  pub(super) fn matches_platforms(&self, platforms: Option<&[String]>) -> bool {
    platforms.is_none_or(|platforms| {
      platforms
        .iter()
        .any(|platform| matches(platform, &self.os_type, &self.os_arch))
    })
  }
}
