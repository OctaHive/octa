use std::sync::{Arc, Weak};

use octa_output::{Console, ConsoleLevel};
use tracing::{
  field::{Field, Visit},
  span::{Attributes, Id},
  Event, Subscriber,
};
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

/// Bridges internal tracing events into the same ordered output stream as task output.
pub struct ConsoleLayer {
  console: Weak<Console>,
}

impl ConsoleLayer {
  pub fn new(console: &Arc<Console>) -> Self {
    Self {
      console: Arc::downgrade(console),
    }
  }
}

impl<S> Layer<S> for ConsoleLayer
where
  S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
  fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
    let mut visitor = RunIdVisitor::default();
    attributes.record(&mut visitor);
    if let (Some(run_id), Some(span)) = (visitor.run_id, context.span(id)) {
      span.extensions_mut().insert(RunContext { run_id });
    }
  }

  fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
    let Some(console) = self.console.upgrade() else {
      return;
    };
    let mut visitor = MessageVisitor::default();
    event.record(&mut visitor);
    let level = level(event.metadata().level());
    let message = visitor.finish();
    let result = match event_run_id(event, &context) {
      Some(run_id) => console.run_message_nowait(run_id, level, message),
      None => console.message_nowait(level, message),
    };
    let _ = result;
  }
}

#[derive(Clone, Copy)]
struct RunContext {
  run_id: u64,
}

#[derive(Default)]
struct RunIdVisitor {
  run_id: Option<u64>,
}

impl Visit for RunIdVisitor {
  fn record_u64(&mut self, field: &Field, value: u64) {
    if field.name() == "run_id" {
      self.run_id = Some(value);
    }
  }

  fn record_debug(&mut self, _field: &Field, _value: &dyn std::fmt::Debug) {}
}

fn event_run_id<S>(event: &Event<'_>, context: &Context<'_, S>) -> Option<u64>
where
  S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
  // Walking root-to-leaf makes the nearest execution span authoritative when
  // one executor is nested inside another.
  let mut run_id = None;
  for span in context.event_scope(event)?.from_root() {
    if let Some(execution) = span.extensions().get::<RunContext>() {
      run_id = Some(execution.run_id);
    }
  }
  run_id
}

#[derive(Default)]
struct MessageVisitor {
  message: Option<String>,
  fields: Vec<String>,
}

impl MessageVisitor {
  fn record(&mut self, field: &Field, value: String) {
    if field.name() == "message" {
      self.message = Some(value);
    } else {
      self.fields.push(format!("{}={value}", field.name()));
    }
  }

  fn finish(self) -> String {
    let mut message = self.message.unwrap_or_default();
    for field in self.fields {
      if !message.is_empty() {
        message.push(' ');
      }
      message.push_str(&field);
    }
    message
  }
}

impl Visit for MessageVisitor {
  fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
    self.record(field, format!("{value:?}"));
  }

  fn record_str(&mut self, field: &Field, value: &str) {
    self.record(field, value.to_owned());
  }
}

fn level(level: &tracing::Level) -> ConsoleLevel {
  match *level {
    tracing::Level::ERROR => ConsoleLevel::Error,
    tracing::Level::WARN => ConsoleLevel::Warn,
    tracing::Level::INFO => ConsoleLevel::Info,
    tracing::Level::DEBUG => ConsoleLevel::Debug,
    tracing::Level::TRACE => ConsoleLevel::Trace,
  }
}

#[cfg(test)]
mod tests {
  use std::{io, sync::Mutex};

  use octa_output::{ConsoleEntry, ConsoleRecord, ConsoleRenderer};
  use tracing_subscriber::prelude::*;

  use super::*;

  struct RecordingRenderer(Arc<Mutex<Vec<ConsoleRecord>>>);

  impl ConsoleRenderer for RecordingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.lock().unwrap().push(entry.record().clone());
      Ok(())
    }
  }

  #[tokio::test]
  async fn forwards_levels_messages_and_fields() {
    let records = Arc::new(Mutex::new(Vec::new()));
    let console = Arc::new(Console::new(RecordingRenderer(records.clone())));
    let subscriber = tracing_subscriber::registry()
      .with(tracing_subscriber::filter::LevelFilter::TRACE)
      .with(ConsoleLayer::new(&console));

    tracing::subscriber::with_default(subscriber, || {
      tracing::error!(message = "error", detail = "fatal");
      tracing::warn!("warn");
      tracing::info!(answer = 42, "formatted {}", "message");
      tracing::debug!("debug");
      tracing::trace!("trace");
      let run = tracing::info_span!("task_execution", run_id = 7_u64);
      run.in_scope(|| {
        tracing::info!("run diagnostic");
        let nested_run = tracing::info_span!("nested_task_execution", run_id = 8_u64);
        nested_run.in_scope(|| tracing::warn!("nested run diagnostic"));
      });
    });
    console.message(ConsoleLevel::Info, "barrier").await.unwrap();

    let records = records.lock().unwrap();
    assert!(matches!(
      &records[0],
      ConsoleRecord::Diagnostic(diagnostic)
        if diagnostic.level == ConsoleLevel::Error
          && diagnostic.message.contains("error")
          && diagnostic.message.contains("detail=fatal")
    ));
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic)
        if diagnostic.level == ConsoleLevel::Info
          && diagnostic.message.contains("formatted message")
          && diagnostic.message.contains("answer=42")
    )));
    assert_eq!(
      records
        .iter()
        .filter(
          |record| matches!(record, ConsoleRecord::Diagnostic(diagnostic) if diagnostic.level == ConsoleLevel::Debug)
        )
        .count(),
      1
    );
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic) if diagnostic.level == ConsoleLevel::Trace
    )));
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic)
        if diagnostic.run_id == Some(7) && diagnostic.message == "run diagnostic"
    )));
    assert!(records.iter().any(|record| matches!(
      record,
      ConsoleRecord::Diagnostic(diagnostic)
        if diagnostic.run_id == Some(8) && diagnostic.message == "nested run diagnostic"
    )));
  }

  #[test]
  fn ignores_events_after_the_console_is_dropped() {
    let layer = {
      let console = Arc::new(Console::new(RecordingRenderer(Arc::new(Mutex::new(Vec::new())))));
      ConsoleLayer::new(&console)
    };
    let subscriber = tracing_subscriber::registry().with(layer);

    tracing::subscriber::with_default(subscriber, || tracing::info!("ignored"));
  }

  #[test]
  fn visitor_handles_field_only_events() {
    let mut visitor = MessageVisitor::default();
    visitor.fields.push("answer=42".to_owned());

    assert_eq!(visitor.finish(), "answer=42");
  }
}
