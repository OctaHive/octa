//! Asynchronous Tera rendering backed by plugin evaluations.

use std::{
  collections::HashMap,
  error::Error,
  sync::{Arc, Mutex},
};

use serde_json::Value;
use tera::{Context, Filter, Function, Tera};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use crate::{
  error::{ExecutorError, ExecutorResult},
  plugin::{EvaluationRequest, PluginEvaluator, PluginExecutionContext, PluginTarget},
};

use octa_plugin::SHELL_CAPABILITY;

/// Values inherited by a plugin invoked while a template is being rendered.
#[derive(Clone)]
pub(crate) struct PluginTemplateContext {
  evaluator: Option<Arc<dyn PluginEvaluator>>,
  execution: PluginExecutionContext,
  cancel_token: CancellationToken,
}

impl PluginTemplateContext {
  pub(crate) fn new(
    evaluator: Option<Arc<dyn PluginEvaluator>>,
    execution: PluginExecutionContext,
    cancel_token: CancellationToken,
  ) -> Self {
    Self {
      evaluator,
      execution,
      cancel_token,
    }
  }

  pub(crate) async fn evaluate(&self, target: PluginTarget, value: Value) -> ExecutorResult<String> {
    let Some(evaluator) = &self.evaluator else {
      if self.execution.dry {
        return Ok(String::new());
      }
      return Err(ExecutorError::PluginEvaluationUnavailable(target.name().to_owned()));
    };

    evaluator
      .evaluate(
        EvaluationRequest {
          target,
          value,
          context: self.execution.clone(),
        },
        self.cancel_token.clone(),
      )
      .await
  }
}

#[derive(Clone, Copy)]
enum HelperKind {
  Plugin,
  Shell,
}

#[derive(Clone)]
struct PluginHelper {
  kind: HelperKind,
  runtime: Handle,
  context: Arc<PluginTemplateContext>,
}

impl PluginHelper {
  fn execute(&self, target: PluginTarget, value: Value) -> tera::Result<Value> {
    self
      .runtime
      .block_on(self.context.evaluate(target, value))
      .map(Value::String)
      .map_err(|error| tera::Error::msg(error.to_string()))
  }

  fn function_request(&self, mut args: HashMap<String, Value>) -> tera::Result<(PluginTarget, Value)> {
    match self.kind {
      HelperKind::Shell => {
        let value = args
          .remove("command")
          .ok_or_else(|| tera::Error::msg("Missing 'command' argument"))?;
        if !args.is_empty() {
          return Err(tera::Error::msg("The 'shell' function only accepts 'command'"));
        }
        Ok((PluginTarget::Capability(SHELL_CAPABILITY.to_owned()), value))
      },
      HelperKind::Plugin => {
        let key = string_argument(&mut args, "key")?;
        let value = args
          .remove("value")
          .ok_or_else(|| tera::Error::msg("Missing 'value' argument"))?;
        if !args.is_empty() {
          return Err(tera::Error::msg("The 'plugin' function only accepts 'key' and 'value'"));
        }
        Ok((PluginTarget::Key(key), value))
      },
    }
  }
}

impl Function for PluginHelper {
  fn call(&self, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let (target, value) = self.function_request(args.clone())?;
    self.execute(target, value)
  }
}

impl Filter for PluginHelper {
  fn filter(&self, value: &Value, args: &HashMap<String, Value>) -> tera::Result<Value> {
    let mut args = args.clone();
    let target = match self.kind {
      HelperKind::Shell => {
        if !args.is_empty() {
          return Err(tera::Error::msg("The 'shell' filter does not accept arguments"));
        }
        PluginTarget::Capability(SHELL_CAPABILITY.to_owned())
      },
      HelperKind::Plugin => {
        let key = string_argument(&mut args, "key")?;
        if !args.is_empty() {
          return Err(tera::Error::msg("The 'plugin' filter only accepts 'key'"));
        }
        PluginTarget::Key(key)
      },
    };

    self.execute(target, value.clone())
  }
}

fn string_argument(args: &mut HashMap<String, Value>, name: &str) -> tera::Result<String> {
  args
    .remove(name)
    .ok_or_else(|| tera::Error::msg(format!("Missing '{name}' argument")))?
    .as_str()
    .map(str::to_owned)
    .ok_or_else(|| tera::Error::msg(format!("'{name}' must be a string")))
}

/// Reuses a configured Tera instance and immutable value context across related templates.
#[derive(Clone)]
pub(crate) struct TemplateRenderer {
  tera: Arc<Mutex<Tera>>,
  context: Arc<Context>,
  plugin_context: Arc<PluginTemplateContext>,
}

impl TemplateRenderer {
  pub(crate) fn new(context: Context, plugin_context: PluginTemplateContext) -> Self {
    let mut tera = Tera::default();
    let runtime = Handle::current();
    let plugin_context = Arc::new(plugin_context);
    let plugin = PluginHelper {
      kind: HelperKind::Plugin,
      runtime: runtime.clone(),
      context: plugin_context.clone(),
    };
    let shell = PluginHelper {
      kind: HelperKind::Shell,
      runtime,
      context: plugin_context.clone(),
    };
    tera.register_function("plugin", plugin.clone());
    tera.register_filter("plugin", plugin);
    tera.register_function("shell", shell.clone());
    tera.register_filter("shell", shell);
    Self {
      tera: Arc::new(Mutex::new(tera)),
      context: Arc::new(context),
      plugin_context,
    }
  }

  /// Runs synchronous Tera work off the async scheduler; plugin callbacks re-enter through the runtime handle.
  pub(crate) async fn render(&self, template: impl Into<String>) -> Result<String, String> {
    let template = template.into();
    let tera = self.tera.clone();
    let context = self.context.clone();
    tokio::task::spawn_blocking(move || {
      tera
        .lock()
        .map_err(|error| error.to_string())?
        .render_str(&template, &context)
        .map_err(|error| format_tera_error(&error))
    })
    .await
    .map_err(|error| error.to_string())?
  }

  pub(crate) async fn evaluate(&self, target: PluginTarget, value: Value) -> ExecutorResult<String> {
    self.plugin_context.evaluate(target, value).await
  }
}

/// Includes nested function errors that Tera omits from its top-level display message.
pub(crate) fn format_tera_error(error: &tera::Error) -> String {
  let mut messages = vec![error.to_string()];
  let mut source = error.source();

  while let Some(error) = source {
    messages.push(error.to_string());
    source = error.source();
  }

  messages.join(": ")
}

#[cfg(test)]
mod tests {
  use async_trait::async_trait;

  use super::*;

  struct EchoEvaluator;

  #[async_trait]
  impl PluginEvaluator for EchoEvaluator {
    async fn evaluate(&self, request: EvaluationRequest, _cancel_token: CancellationToken) -> ExecutorResult<String> {
      Ok(format!("{}:{}", request.target.name(), request.value.as_str().unwrap()))
    }
  }

  fn plugin_context() -> PluginTemplateContext {
    PluginTemplateContext::new(
      Some(Arc::new(EchoEvaluator)),
      PluginExecutionContext {
        dir: std::path::PathBuf::from("."),
        vars: HashMap::new(),
        envs: HashMap::new(),
        secret_vars: Vec::new(),
        dry: false,
        redact_params: false,
      },
      CancellationToken::new(),
    )
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn renders_generic_plugin_function_and_filter() {
    let context = Context::new();

    let renderer = TemplateRenderer::new(context, plugin_context());
    let rendered = renderer
      .render(r#"{{ plugin(key="tpl", value="function") }} {{ "filter" | plugin(key="tpl") }}"#)
      .await
      .unwrap();

    assert_eq!(rendered, "tpl:function tpl:filter");
  }

  #[tokio::test(flavor = "multi_thread")]
  async fn shell_helpers_delegate_to_the_shell_capability() {
    let renderer = TemplateRenderer::new(Context::new(), plugin_context());
    let rendered = renderer
      .render(r#"{{ shell(command="function") }} {{ "filter" | shell }}"#)
      .await
      .unwrap();

    assert_eq!(rendered, "shell:function shell:filter");
  }
}
