mod error;
mod include;
mod monorepo;
mod octafile;
mod parser;
mod task;
mod variable;

pub use error::{OctafileError, OctafileResult};
pub use include::IncludeInfo;
pub use monorepo::{read_monorepo_config, MonorepoConfig};
pub use octafile::{EnvValue, Envs, Octafile, ShellValue, SyntheticInclude, Vars, WatchInterval};
pub use task::{
  AllowedRun, CommandOptions, CommandPayload, ComplexDep, ConditionEvaluation, Deps, ExecuteMode, PluginCommand,
  PluginSchemas, SourceStrategies, Task, TaskCommand, TaskCondition, TaskConditions, Timeout,
};
pub use variable::{RequiredMode, Variable, VariableSource};
