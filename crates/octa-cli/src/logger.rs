use chrono::Local;
use nu_ansi_term::{Color, Style};
use tracing_subscriber::{
  fmt::{self, format::Writer, time::FormatTime, FormatEvent, FormatFields},
  prelude::*,
  registry::LookupSpan,
};

pub struct ChronoLocal;

impl FormatTime for ChronoLocal {
  fn format_time(&self, w: &mut fmt::format::Writer<'_>) -> std::fmt::Result {
    write!(
      w,
      "{}",
      Style::new()
        .fg(Color::DarkGray)
        .paint(Local::now().format("%Y-%m-%d %H:%M:%S").to_string())
    )
  }
}

struct OctaFieldFormatter<'a> {
  level: &'a tracing::Level, // Example of custom context
}

impl<'writer> FormatFields<'writer> for OctaFieldFormatter<'_> {
  fn format_fields<R: __tracing_subscriber_field_RecordFields>(
    &self,
    writer: Writer<'writer>,
    fields: R,
  ) -> std::fmt::Result {
    let mut visitor = OctaFieldVisitor::new(writer, self.level);
    fields.record(&mut visitor);
    Ok(())
  }
}

struct OctaFieldVisitor<'a, 'writer> {
  writer: Writer<'writer>,
  level: &'a tracing::Level,
  result: std::fmt::Result,
}

impl<'a, 'writer> OctaFieldVisitor<'a, 'writer> {
  fn new(writer: Writer<'writer>, level: &'a tracing::Level) -> Self {
    Self {
      writer,
      level,
      result: Ok(()),
    }
  }
}

impl tracing::field::Visit for OctaFieldVisitor<'_, '_> {
  fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
    if self.result.is_err() {
      return;
    }

    if field.name() == "message" {
      // Use level context to determine message color
      let style = match *self.level {
        tracing::Level::ERROR => Style::new().fg(Color::Red),
        tracing::Level::WARN => Style::new().fg(Color::Yellow),
        tracing::Level::INFO => Style::new().fg(Color::Green),
        tracing::Level::DEBUG => Style::new().fg(Color::Blue),
        tracing::Level::TRACE => Style::new().fg(Color::Purple),
      };

      self.result = write!(self.writer, "{}", style.paint(format!("{:?}", value)));
    } else {
      self.result = write!(self.writer, "{:?}", value);
    }
  }

  fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
    if self.result.is_err() {
      return;
    }

    if field.name() == "message" {
      // Use level context to determine message color
      let style = match *self.level {
        tracing::Level::ERROR => Style::new().fg(Color::Red),
        tracing::Level::WARN => Style::new().fg(Color::Yellow),
        tracing::Level::INFO => Style::new().fg(Color::White),
        tracing::Level::DEBUG => Style::new().fg(Color::Blue),
        tracing::Level::TRACE => Style::new().fg(Color::Purple),
      };

      self.result = write!(self.writer, "{}", style.paint(value));
    } else {
      self.result = write!(self.writer, "{}", value);
    }
  }
}

// Custom formatter for adding [octa] prefix
pub struct OctaFormatter;

impl<S, N> FormatEvent<S, N> for OctaFormatter
where
  S: tracing::Subscriber + for<'a> LookupSpan<'a>,
  N: for<'writer> FormatFields<'writer> + 'static,
{
  fn format_event(
    &self,
    _ctx: &fmt::FmtContext<'_, S, N>,
    mut writer: Writer<'_>,
    event: &tracing::Event<'_>,
  ) -> std::fmt::Result {
    let timer = ChronoLocal;
    timer.format_time(&mut writer)?;
    writer.write_char(' ')?;

    // Add [octa] prefix with styling
    write!(
      writer,
      "{} ",
      Style::new().on(Color::Blue).fg(Color::Yellow).paint("[octa]")
    )?;

    let level = *event.metadata().level();
    let formatter = OctaFieldFormatter { level: &level };
    formatter.format_fields(writer.by_ref(), event)?;

    writeln!(writer)
  }
}
