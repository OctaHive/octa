//! Variable, environment, and working-directory inheritance for task invocations.

use super::*;

impl TaskGraphBuilder {
  pub(super) fn collect_vars_with_identity(
    &self,
    cmd: &FindResult,
    execute_vars: Option<octa_octafile::Vars>,
  ) -> ExecutorResult<CollectedVars> {
    let mut vars = self.initialize_global_vars(cmd);

    self.process_hierarchy_vars(cmd, &mut vars)?;
    vars = self.add_task_vars(cmd, vars);
    if let Some(exec_vars) = execute_vars {
      vars.extend_variables(exec_vars);
    }

    // Ambient values are available at runtime, but only explicitly declared
    // names are build inputs. This avoids invalidating every task when an
    // unrelated process variable changes.
    let mut identity_names = vars.declared_names();
    let env_vars: HashMap<String, String> = env::vars().collect();
    vars.extend_with(&env_vars);

    if !self.variable_overrides.is_empty() {
      // Keep overrides in their own layer so their templates can consume every configured value,
      // while declaration order remains meaningful between CLI options.
      let mut overrides = Vars::with_parent(vars);
      for (name, value) in &self.variable_overrides {
        overrides.insert(name, value);
        identity_names.insert(name.clone());
      }
      vars = overrides;
    }

    Ok(CollectedVars {
      runtime: vars,
      identity_names,
    })
  }

  fn initialize_global_vars(&self, cmd: &FindResult) -> Vars {
    let os_type = whoami::platform();
    let os_arch = whoami::cpu_arch();
    let root = cmd.octafile.root();

    let mut vars = root.vars.clone().map(Vars::with_variables).unwrap_or_default();
    vars.set_dir(root.dir.clone());

    vars.insert("ROOT_DIR", &root.dir.display().to_string());
    vars.insert("OCTAFILE_DIR", &root.dir.display().to_string());
    vars.insert("USER_WORKING_DIR", &self.dir.display().to_string());
    vars.insert("COMMAND_ARGS", &self.command_args);
    vars.insert("OCTA_OS", &os_type.to_string());
    vars.insert("OCTA_ARCH", &os_arch.to_string());

    vars
  }

  pub(super) fn process_hierarchy_vars(&self, cmd: &FindResult, vars: &mut Vars) -> ExecutorResult<()> {
    let full_path = cmd.octafile.hierarchy_path();
    let mut current = Arc::clone(cmd.octafile.root());

    debug!(
      "Processing hierarchy variables for command {} in path {}",
      cmd.name,
      full_path.join(":")
    );

    for segment in full_path {
      match current.get_included(&segment)? {
        Some(nested_octafile) => {
          let mut new_vars = Vars::new();
          new_vars.set_parent(Some(vars.clone()));
          if let Some(nested_vars) = nested_octafile.vars.clone() {
            new_vars.set_variables(nested_vars);
          }
          new_vars.set_dir(nested_octafile.dir.clone());
          new_vars.insert("TASKFILE_DIR", &current.dir.display().to_string());

          *vars = new_vars;
          current = Arc::clone(&nested_octafile);
          debug!("Updated variables for segment {}", segment);
        },
        None => {
          debug!("No nested octafile found for segment {}", segment);
          break;
        },
      }
    }

    Ok(())
  }

  fn add_task_vars(&self, cmd: &FindResult, vars: Vars) -> Vars {
    // Add variables from current task
    match cmd.task.vars.clone() {
      Some(task_vars) => {
        let mut new_vars = Vars::with_variables_and_parent(task_vars, vars);
        new_vars.set_dir(self.variable_working_dir(cmd));

        new_vars
      },
      None => {
        let mut new_vars = Vars::new();
        new_vars.set_parent(Some(vars));
        new_vars.set_dir(self.variable_working_dir(cmd));

        new_vars
      },
    }
  }

  /// Collects environments from global, hierarchy and task levels
  pub(super) fn collect_environment_plan(
    &self,
    cmd: &FindResult,
    execute_envs: Option<octa_octafile::Envs>,
  ) -> ExecutorResult<EnvironmentPlan> {
    let mut plan = EnvironmentPlan::default();
    let root = cmd.octafile.root();
    plan.push_layer(root.env.clone(), root.dir.clone(), root.dotenv.clone());

    let full_path = cmd.octafile.hierarchy_path();
    let mut current = Arc::clone(root);
    for segment in full_path {
      match current.get_included(&segment)? {
        Some(nested_octafile) => {
          plan.push_layer(
            nested_octafile.env.clone(),
            nested_octafile.dir.clone(),
            nested_octafile.dotenv.clone(),
          );
          current = nested_octafile;
        },
        None => break,
      }
    }

    plan.push_layer(
      cmd.task.env.clone(),
      self.task_working_dir(cmd),
      cmd.task.dotenv.clone(),
    );
    if let Some(exec_vars) = execute_envs {
      plan.extend_last(exec_vars);
    }
    Ok(plan)
  }

  pub(super) fn task_working_dir(&self, cmd: &FindResult) -> PathBuf {
    let task_dir = cmd.task.dir.as_ref().unwrap_or(&cmd.octafile.dir);
    if task_dir.is_absolute() {
      task_dir.clone()
    } else {
      self.dir.join(task_dir)
    }
  }

  pub(super) fn variable_working_dir(&self, cmd: &FindResult) -> PathBuf {
    let task_dir = self.task_working_dir(cmd);
    let value = task_dir.to_string_lossy();
    // A templated task directory cannot be resolved until variables have been expanded.
    if value.contains("{{") && value.contains("}}") {
      cmd.octafile.dir.clone()
    } else {
      task_dir
    }
  }
}
