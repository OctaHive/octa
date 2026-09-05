mod error;
mod include;
mod monorepo;
mod octafile;
mod output;
mod parser;
mod task;
mod variable;

pub use error::{OctafileError, OctafileResult};
pub use include::IncludeInfo;
pub use monorepo::{read_monorepo_config, MonorepoConfig};
pub use octafile::{EnvValue, Envs, Octafile, PresentationConfig, ShellValue, SyntheticInclude, Vars, WatchInterval};
pub use output::{GroupOutput, OutputConfig, OutputMode, TaskOutputMode, TaskPresentation};
pub use task::{
  AllowedRun, CommandOptions, CommandPayload, ComplexDep, ConditionEvaluation, Deps, ExecuteMode, PluginCommand,
  PluginSchemas, Silence, SourceStrategies, Task, TaskCommand, TaskCondition, TaskConditions, Timeout,
};
pub use variable::{RequiredMode, Variable, VariableEnum, VariableSource};
