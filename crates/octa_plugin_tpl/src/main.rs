use std::{borrow::Cow, collections::HashMap, env, path::PathBuf, sync::Arc};

use anyhow::Context;
use async_trait::async_trait;
use serde_json::Value;

use octa_plugin::{logger::Logger, protocol::ServerResponse, serve_plugin, Plugin};
use tera::{Context as TeraContext, Tera};
use tokio::{
  io::{AsyncWrite, AsyncWriteExt},
  sync::Mutex,
};
use tokio_util::sync::CancellationToken;

struct TemplatePlugin {}

#[async_trait]
impl Plugin for TemplatePlugin {
  /// Return plugin version
  fn version(&self) -> String {
    env!("CARGO_PKG_VERSION").to_owned()
  }

  async fn execute_command(
    &self,
    id: String,
    command: String,
    _args: Vec<String>,
    _dir: PathBuf,
    vars: HashMap<String, Value>,
    envs: HashMap<String, String>,
    writer: Arc<Mutex<impl AsyncWrite + Send + 'static + std::marker::Unpin>>,
    logger: Arc<impl Logger>,
    _cancel_token: CancellationToken,
  ) -> anyhow::Result<()> {
    logger.log("Start processing template command")?;

    let mut tera = Tera::default();
    let template_name = format!("template_{}", id);

    let get_env = |name: &str| match envs.get(name) {
      Some(val) => Some(Cow::Borrowed(val.as_str())),
      None => match env::var(name) {
        Ok(val) => Some(Cow::Owned(val)),
        Err(_) => None,
      },
    };

    let val = shellexpand::env_with_context_no_errors(&command, get_env);

    tera
      .add_raw_template(&template_name, val.as_ref())
      .context("Failed to parse template")?;

    let context = TeraContext::from_serialize(vars)?;

    let result = tera
      .render(&template_name, &context)
      .context(format!("Failed to render template: {:?}", context))?;

    let mut lock = writer.lock().await;

    let response = ServerResponse::Stdout {
      id: id.clone(),
      line: result,
    };
    let response_json = serde_json::to_string(&response).unwrap() + "\n";
    let _ = lock.write_all(response_json.as_bytes()).await;

    let response = ServerResponse::ExitStatus {
      id: id.clone(),
      code: 0,
    };
    let response_json = serde_json::to_string(&response).unwrap() + "\n";
    let _ = lock.write_all(response_json.as_bytes()).await;

    let _ = lock.flush().await;

    Ok(())
  }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  serve_plugin(TemplatePlugin {}).await
}
