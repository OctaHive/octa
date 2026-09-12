//! Construction of portable task action identities.

use std::{collections::BTreeMap, env, path::Path};

use octa_cache_protocol::{
  ActionDescriptorV1, Digest, DigestAlgorithm, PluginIdentity, RelativePath, ACTION_KEY_FORMAT_V1,
};
use serde_json::{Map, Value};

use super::{ResultCache, TaskCachePlan};
use crate::{
  error::{ExecutorError, ExecutorResult},
  task::RuntimeContext,
};

pub(super) async fn action_digest(
  cache: &ResultCache,
  plan: &TaskCachePlan,
  plugin_manager: &octa_plugin_manager::plugin_manager::PluginManager,
  context: &RuntimeContext,
  input_root: Digest,
) -> ExecutorResult<Digest> {
  action_descriptor(cache, plan, plugin_manager, context, input_root)
    .await?
    .digest()
    .map_err(ExecutorError::from)
}

async fn action_descriptor(
  cache: &ResultCache,
  plan: &TaskCachePlan,
  plugin_manager: &octa_plugin_manager::plugin_manager::PluginManager,
  context: &RuntimeContext,
  input_root: Digest,
) -> ExecutorResult<ActionDescriptorV1> {
  let working_directory = relative_directory(&plan.workspace, &context.dir)?;
  let variables = digest_json(&context.vars.action_values(Some(&context.identity_names)))?;
  let configured = std::collections::HashMap::<String, String>::from(context.envs.clone())
    .into_iter()
    .collect::<BTreeMap<_, _>>();
  let mut inherited = BTreeMap::<String, Option<String>>::new();
  for name in &plan.environment {
    // Missing and explicitly empty values have different process semantics. A
    // selected non-Unicode value cannot be represented portably and therefore
    // fails closed instead of being mistaken for a missing variable.
    let value = env::var_os(name)
      .map(|value| {
        value.into_string().map_err(|_| {
          ExecutorError::ActionIdentityError(format!("cache environment variable '{name}' is not valid Unicode"))
        })
      })
      .transpose()?;
    inherited.insert(name.clone(), value);
  }
  let plugins = plugin_identities(plugin_manager, plan, context).await?;
  Ok(ActionDescriptorV1 {
    key_format: ACTION_KEY_FORMAT_V1,
    task_definition: plan.task_definition,
    input_root,
    working_directory,
    variables,
    environment: digest_json(&serde_json::json!({
      "configured": configured,
      "inherited": inherited,
    }))?,
    runtime: cache.runtime.clone(),
    plugins,
    arguments: plan.arguments.clone(),
    timeout: plan.timeout,
    salt: plan.salt.clone(),
  })
}

async fn plugin_identities(
  manager: &octa_plugin_manager::plugin_manager::PluginManager,
  plan: &TaskCachePlan,
  context: &RuntimeContext,
) -> ExecutorResult<Vec<PluginIdentity>> {
  let mut selectors = context
    .plugin_uses
    .lock()
    .map_err(|_| ExecutorError::LockError("plugin use tracker poisoned".to_owned()))?
    .clone();
  selectors.extend(plan.plugin_keys.iter().cloned().map(crate::plugin::PluginTarget::Key));

  let mut identities = BTreeMap::new();
  for selector in selectors {
    let identity = match &selector {
      crate::plugin::PluginTarget::Key(key) => manager.identity_for_key(key).await,
      crate::plugin::PluginTarget::Capability(capability) => manager.identity_for_capability(capability).await,
    }
    .map_err(|error| ExecutorError::ActionIdentityError(error.to_string()))?
    .ok_or_else(|| ExecutorError::PluginUnavailable(selector.name().to_owned()))?;
    let executable = Digest::from_hex(DigestAlgorithm::Sha256, &identity.sha256, identity.executable_size)
      .map_err(ExecutorError::from)?;
    identities.insert(
      identity.name.clone(),
      PluginIdentity {
        name: identity.name,
        version: identity.version,
        protocol_version: identity.protocol_version,
        executable,
      },
    );
  }
  Ok(identities.into_values().collect())
}

pub(super) fn relative_directory(workspace: &Path, directory: &Path) -> ExecutorResult<RelativePath> {
  let workspace = dunce::canonicalize(workspace)?;
  let directory = dunce::canonicalize(directory)?;
  let relative = directory.strip_prefix(&workspace).map_err(|_| {
    ExecutorError::InvalidCacheConfiguration("task working directory is outside the cache workspace".to_owned())
  })?;
  if relative.as_os_str().is_empty() {
    return Ok(RelativePath::root());
  }
  RelativePath::new(relative.to_string_lossy().replace('\\', "/")).map_err(ExecutorError::from)
}

pub(super) fn digest_json(value: &impl serde::Serialize) -> ExecutorResult<Digest> {
  let value = serde_json::to_value(value).map_err(ExecutorError::ActionIdentitySerialization)?;
  let bytes = serde_json::to_vec(&canonical_json(&value)).map_err(ExecutorError::ActionIdentitySerialization)?;
  Ok(Digest::new(
    DigestAlgorithm::Blake3,
    *blake3::hash(&bytes).as_bytes(),
    bytes.len() as u64,
  ))
}

fn canonical_json(value: &Value) -> Value {
  match value {
    Value::Object(values) => {
      let ordered = values
        .iter()
        .map(|(name, value)| (name.clone(), canonical_json(value)))
        .collect::<BTreeMap<_, _>>();
      Value::Object(Map::from_iter(ordered))
    },
    Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
    value => value.clone(),
  }
}
