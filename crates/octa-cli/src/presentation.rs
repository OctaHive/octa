use std::{env, sync::Arc};

use clap::ValueEnum;
use octa_output::{
  Console, ConsoleRenderer, GithubActionsRenderer, GroupRenderer, OnErrorRenderer, PrefixedRenderer, TerminalRenderer,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum OutputMode {
  /// Stream command output as it arrives.
  #[default]
  Interleaved,
  /// Print each completed task invocation as one contiguous block.
  Group,
  /// Prefix each line with its task label while keeping output live.
  Prefixed,
  /// Retain line output only for failed or cancelled task invocations.
  OnError,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum CiMode {
  /// Detect the CI provider from its environment.
  #[default]
  Auto,
  /// Disable CI-specific presentation.
  None,
  /// Emit GitHub Actions workflow annotations.
  Github,
}

/// Builds the terminal presentation pipeline without exposing its composition to CLI orchestration.
pub(crate) fn terminal_console(output: OutputMode, ci: CiMode) -> Arc<Console> {
  let renderer: Box<dyn ConsoleRenderer> = match output {
    OutputMode::Interleaved => Box::new(TerminalRenderer::default()),
    OutputMode::Group => Box::new(GroupRenderer::new(TerminalRenderer::default())),
    OutputMode::Prefixed => Box::new(PrefixedRenderer::new(TerminalRenderer::default())),
    OutputMode::OnError => Box::new(OnErrorRenderer::new(TerminalRenderer::default())),
  };
  let renderer: Box<dyn ConsoleRenderer> = if github_actions_enabled(ci, env::var("GITHUB_ACTIONS").ok().as_deref()) {
    Box::new(GithubActionsRenderer::new(renderer))
  } else {
    renderer
  };
  Arc::new(Console::new(renderer))
}

fn github_actions_enabled(mode: CiMode, environment: Option<&str>) -> bool {
  match mode {
    CiMode::Auto => environment.is_some_and(|value| value.eq_ignore_ascii_case("true")),
    CiMode::None => false,
    CiMode::Github => true,
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn github_actions_can_be_detected_overridden_and_disabled() {
    assert!(github_actions_enabled(CiMode::Auto, Some("true")));
    assert!(github_actions_enabled(CiMode::Auto, Some("TRUE")));
    assert!(!github_actions_enabled(CiMode::Auto, Some("false")));
    assert!(!github_actions_enabled(CiMode::None, Some("true")));
    assert!(github_actions_enabled(CiMode::Github, None));
  }
}
