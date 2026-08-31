mod error;
mod include;
mod octafile;
mod parser;
mod task;
mod variable;

pub use error::{OctafileError, OctafileResult};
pub use include::IncludeInfo;
pub use octafile::{EnvValue, Envs, Octafile, ShellValue, Vars, WatchInterval};
pub use task::{
  AllowedRun, CommandOptions, CommandPayload, ConditionEvaluation, Deps, ExecuteMode, PluginCommand, PluginSchemas,
  SourceStrategies, Task, TaskCommand, TaskCondition, TaskConditions, Timeout,
};
pub use variable::Variable;
