//! Variable, environment, and working-directory inheritance for task invocations.

use super::*;

impl TaskGraphBuilder {
  /// Collects variables from global, hierarchy and task levels.
  pub(super) fn collect_vars(
    &self,
    cmd: &FindResult,
    execute_vars: Option<octa_octafile::Vars>,
  ) -> ExecutorResult<Vars> {
    Ok(self.collect_vars_with_identity(cmd, execute_vars)?.runtime)
  }

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

    vars.resolve_required(self.variable_resolver.as_deref())?;
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
  pub(super) fn collect_envs(
    &self,
    cmd: &FindResult,
    execute_envs: Option<octa_octafile::Envs>,
    vars: &Vars,
  ) -> ExecutorResult<Envs> {
    // Dotenv path templates are resolved while the hierarchy is built, so keep a flattened
    // view of the process environment and all environment layers processed so far.
    let mut template_environment = env::vars().collect::<HashMap<_, _>>();
    let mut envs = self.initialize_global_envs(cmd, vars, &mut template_environment)?;
    self.process_hierarchy_envs(cmd, vars, &mut envs, &mut template_environment)?;
    envs = self.add_task_envs(cmd, vars, envs, &mut template_environment)?;
    if let Some(exec_vars) = execute_envs {
      envs.extend(exec_vars.clone());
    }
    Ok(envs)
  }

  fn initialize_global_envs(
    &self,
    cmd: &FindResult,
    vars: &Vars,
    template_environment: &mut HashMap<String, String>,
  ) -> ExecutorResult<Envs> {
    let mut envs = Envs::new();
    let root = cmd.octafile.root();
    envs.set_dir(root.dir.clone());
    let mut values = Self::dotenv_values(dotenv::load(
      root.dotenv.as_deref(),
      &root.dir,
      vars,
      template_environment,
    )?);

    // Explicit Octafile values take precedence over values loaded from dotenv files.
    if let Some(env) = &root.env {
      values.extend(env.clone());
    }
    Self::extend_template_environment(template_environment, &values);
    envs.set_value(values);

    Ok(envs)
  }

  fn process_hierarchy_envs(
    &self,
    cmd: &FindResult,
    vars: &Vars,
    envs: &mut Envs,
    template_environment: &mut HashMap<String, String>,
  ) -> ExecutorResult<()> {
    let full_path = cmd.octafile.hierarchy_path();
    let mut current = Arc::clone(cmd.octafile.root());

    debug!(
      "Processing hierarchy environments for command {} in path {}",
      cmd.name,
      full_path.join(":")
    );

    for segment in full_path {
      match current.get_included(&segment)? {
        Some(nested_octafile) => {
          let mut new_envs = Envs::new();
          new_envs.set_parent(Some(envs.clone()));
          new_envs.set_dir(nested_octafile.dir.clone());
          let mut values = Self::dotenv_values(dotenv::load(
            nested_octafile.dotenv.as_deref(),
            &nested_octafile.dir,
            vars,
            template_environment,
          )?);
          // Each included Octafile overrides its dotenv values explicitly and then overrides
          // all environment values inherited from its parent.
          if let Some(env) = &nested_octafile.env {
            values.extend(env.clone());
          }
          Self::extend_template_environment(template_environment, &values);
          new_envs.set_value(values);

          *envs = new_envs;
          current = Arc::clone(&nested_octafile);
          debug!("Updated environments for segment {}", segment);
        },
        None => {
          debug!("No nested octafile found for segment {}", segment);
          break;
        },
      }
    }

    Ok(())
  }

  fn add_task_envs(
    &self,
    cmd: &FindResult,
    vars: &Vars,
    envs: Envs,
    template_environment: &mut HashMap<String, String>,
  ) -> ExecutorResult<Envs> {
    let mut new_envs = Envs::new();
    new_envs.set_parent(Some(envs));

    let task_dir = self.task_working_dir(cmd);
    let mut values = Self::dotenv_values(dotenv::load(
      cmd.task.dotenv.as_deref(),
      &task_dir,
      vars,
      template_environment,
    )?);
    // Task values are the most specific layer and therefore override task dotenv values.
    if let Some(task_envs) = &cmd.task.env {
      values.extend(task_envs.clone());
    }
    Self::extend_template_environment(template_environment, &values);
    new_envs.set_value(values);

    Ok(new_envs)
  }

  fn dotenv_values(values: HashMap<String, String>) -> octa_octafile::Envs {
    values
      .into_iter()
      .map(|(key, value)| (key, EnvValue::String(value)))
      .collect()
  }

  fn extend_template_environment(target: &mut HashMap<String, String>, values: &octa_octafile::Envs) {
    // Dynamic values are evaluated only when the task runs and therefore cannot select a
    // dotenv file while the execution graph is being built.
    target.extend(
      values
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_owned()))),
    );
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
