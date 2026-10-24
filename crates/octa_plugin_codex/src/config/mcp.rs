//! Explicit Codex MCP settings. Connections and credentials keep their existing owners.
use super::*;

const MAX_SERVERS: usize = 64;
const MAX_SETTINGS_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum McpServer {
  Http {
    url: String,
    bearer_token_env_var: Option<String>,
    enabled_tools: Option<Vec<String>>,
  },
  Stdio {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env_vars: Vec<String>,
    enabled_tools: Option<Vec<String>>,
  },
}

impl McpServer {
  /// Produces only documented Codex settings; credentials are environment names, never values.
  pub(crate) fn configuration(&self) -> toml::Value {
    let mut table = toml::Table::from_iter([("required".into(), toml::Value::Boolean(true))]);
    let enabled_tools = match self {
      Self::Http {
        url,
        bearer_token_env_var,
        enabled_tools,
      } => {
        table.insert("url".into(), toml::Value::String(url.clone()));
        if let Some(name) = bearer_token_env_var {
          table.insert("bearer_token_env_var".into(), toml::Value::String(name.clone()));
        }
        enabled_tools
      },
      Self::Stdio {
        command,
        args,
        env_vars,
        enabled_tools,
      } => {
        table.insert("command".into(), toml::Value::String(command.clone()));
        table.insert("args".into(), strings(args));
        table.insert("env_vars".into(), strings(env_vars));
        enabled_tools
      },
    };
    if let Some(tools) = enabled_tools {
      table.insert("enabled_tools".into(), strings(tools));
    }
    toml::Value::Table(table)
  }
}

pub(super) fn validate(
  servers: &BTreeMap<String, McpServer>,
  environment: &EnvironmentSelection,
) -> anyhow::Result<()> {
  if servers.len() > MAX_SERVERS {
    bail!("too many MCP servers");
  }
  for (name, server) in servers {
    if name.is_empty()
      || name.len() > 64
      || !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
      bail!("invalid MCP server name");
    }
    let enabled_tools = match server {
      McpServer::Http {
        url,
        bearer_token_env_var,
        enabled_tools,
      } => {
        validate_bounded_text("MCP endpoint", url, MAX_PATH_BYTES)?;
        let endpoint = url::Url::parse(url).map_err(|_| anyhow::anyhow!("invalid MCP endpoint"))?;
        if !matches!(endpoint.scheme(), "http" | "https")
          || endpoint.host_str().is_none()
          || !endpoint.username().is_empty()
          || endpoint.password().is_some()
          || endpoint.fragment().is_some()
        {
          bail!("invalid MCP endpoint");
        }
        if let Some(name) = bearer_token_env_var {
          if endpoint.scheme() != "https" {
            bail!("MCP authentication requires HTTPS");
          }
          validate_environment_name(name)?;
          if !environment.secret.contains_key(name) {
            bail!("MCP authentication requires an explicit secret mapping");
          }
        }
        enabled_tools
      },
      McpServer::Stdio {
        command,
        args,
        env_vars,
        enabled_tools,
      } => {
        validate_bounded_text("MCP command", command, MAX_PATH_BYTES)?;
        if !std::path::Path::new(command).is_absolute()
          || args.len() > MAX_CLI_ARGUMENTS
          || args.iter().any(|arg| arg.len() > MAX_PATH_BYTES || arg.contains('\0'))
          || env_vars.len() > MAX_ENVIRONMENT_MAPPINGS
          || env_vars.iter().collect::<HashSet<_>>().len() != env_vars.len()
        {
          bail!("invalid MCP stdio settings");
        }
        for name in env_vars {
          validate_environment_name(name)?;
          if !environment.public.contains_key(name) && !environment.secret.contains_key(name) {
            bail!("MCP environment requires an explicit mapping");
          }
        }
        enabled_tools
      },
    };
    if let Some(tools) = enabled_tools {
      if tools.len() > MAX_SERVERS || tools.iter().collect::<HashSet<_>>().len() != tools.len() {
        bail!("invalid MCP tool selection");
      }
      for tool in tools {
        validate_name("MCP tool", tool)?;
      }
    }
  }
  if configuration(servers).to_string().len() > MAX_SETTINGS_BYTES {
    bail!("MCP settings exceed the byte limit");
  }
  Ok(())
}

fn strings(values: &[String]) -> toml::Value {
  toml::Value::Array(values.iter().cloned().map(toml::Value::String).collect())
}

/// A single override replaces the MCP map instead of merging ambient server names.
pub(crate) fn configuration(servers: &BTreeMap<String, McpServer>) -> toml::Value {
  toml::Value::Table(
    servers
      .iter()
      .map(|(name, server)| (name.clone(), server.configuration()))
      .collect(),
  )
}

pub(super) fn schema() -> Value {
  let environment_name =
    serde_json::json!({"type":"string", "pattern":"^[A-Za-z_][A-Za-z0-9_]*$", "maxLength":MAX_NAME_BYTES});
  let tools =
    serde_json::json!({"type":"array", "maxItems":MAX_SERVERS, "uniqueItems":true, "items":bounded_name_schema()});
  serde_json::json!({
    "type": "object", "maxProperties": MAX_SERVERS,
    "propertyNames": {"type":"string", "pattern":"^[A-Za-z0-9_-]+$", "maxLength":64},
    "additionalProperties": {"oneOf":[{
      "type":"object", "additionalProperties":false, "required":["transport","url"],
      "properties": {
        "transport":{"const":"http"}, "url":bounded_text_schema(MAX_PATH_BYTES),
        "bearer_token_env_var":environment_name, "enabled_tools":tools
      }
    },{
      "type":"object", "additionalProperties":false, "required":["transport","command"],
      "properties":{
        "transport":{"const":"stdio"}, "command":bounded_text_schema(MAX_PATH_BYTES),
        "args":{"type":"array", "maxItems":MAX_CLI_ARGUMENTS, "items":{"type":"string", "maxLength":MAX_PATH_BYTES, "not":{"pattern":"\\u0000"}}},
        "env_vars":{"type":"array", "maxItems":MAX_ENVIRONMENT_MAPPINGS, "uniqueItems":true, "items":environment_name},
        "enabled_tools":tools
      }
    }]}
  })
}
