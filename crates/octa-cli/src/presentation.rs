use std::{env, io::IsTerminal, sync::Arc};

use clap::ValueEnum;
use octa_octafile::{OutputConfig, OutputMode};
use octa_output::{
  Console, ConsoleRenderer, GithubActionsRenderer, OutputRouterConfig, OutputRouterRenderer, QuietRenderer, RenderMode,
};

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
pub(crate) fn terminal_console(
  output: OutputConfig,
  ci: CiMode,
  quiet: bool,
  force_output_mode: bool,
  adaptive_output: bool,
) -> Arc<Console> {
  let mode = render_mode(output.mode);
  let github =
    output.mode != OutputMode::Json && github_actions_enabled(ci, env::var("GITHUB_ACTIONS").ok().as_deref());
  let progress_enabled = std::io::stderr().is_terminal() && !github && !ci_environment(env::var("CI").ok().as_deref());
  let renderer: Box<dyn ConsoleRenderer> = Box::new(OutputRouterRenderer::new(OutputRouterConfig {
    default_mode: mode,
    group_begin: output.group.begin,
    group_end: output.group.end,
    group_error_only: output.group.error_only,
    progress_enabled,
    adaptive_default: adaptive_output,
    force_default: force_output_mode,
  }));
  let renderer: Box<dyn ConsoleRenderer> = if quiet {
    Box::new(QuietRenderer::new(renderer))
  } else {
    renderer
  };
  let renderer: Box<dyn ConsoleRenderer> = if github {
    Box::new(GithubActionsRenderer::new(renderer))
  } else {
    renderer
  };
  Arc::new(Console::new(renderer))
}

fn render_mode(mode: OutputMode) -> RenderMode {
  match mode {
    OutputMode::Interleaved => RenderMode::Interleaved,
    OutputMode::Group => RenderMode::Group,
    OutputMode::Prefixed => RenderMode::Prefixed,
    OutputMode::OnError => RenderMode::OnError,
    OutputMode::KeepOrder => RenderMode::KeepOrder,
    OutputMode::Replacing => RenderMode::Replacing,
    OutputMode::Timed => RenderMode::Timed,
    OutputMode::Json => RenderMode::Json,
  }
}

fn github_actions_enabled(mode: CiMode, environment: Option<&str>) -> bool {
  match mode {
    CiMode::Auto => environment.is_some_and(|value| value.eq_ignore_ascii_case("true")),
    CiMode::None => false,
    CiMode::Github => true,
  }
}

fn ci_environment(value: Option<&str>) -> bool {
  value.is_some_and(|value| !matches!(value.trim().to_ascii_lowercase().as_str(), "" | "0" | "false" | "no"))
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

  #[test]
  fn progress_treats_truthy_ci_values_as_unattended() {
    assert!(ci_environment(Some("true")));
    assert!(ci_environment(Some("1")));
    assert!(!ci_environment(Some("false")));
    assert!(!ci_environment(Some(" FALSE ")));
    assert!(!ci_environment(Some("0")));
    assert!(!ci_environment(None));
  }
}
