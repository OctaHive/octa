use std::io::{self, IsTerminal, Write};

use chrono::{DateTime, Local, Utc};
use humanize_duration::{prelude::DurationExt, Truncate};
use nu_ansi_term::{Color, Style};

use super::{
  CliDocument, ConsoleDiagnostic, ConsoleEntry, ConsoleLevel, ConsolePayload, ConsoleRecord, ConsoleRenderer,
  ConsoleStatus, ConsoleStream, ExecutionEvent,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalColor {
  #[default]
  Auto,
  Always,
  Never,
}

/// Presents structured console records on the process standard streams.
pub struct TerminalRenderer {
  styled: bool,
  stdout: Box<dyn Write + Send>,
  stderr: Box<dyn Write + Send>,
}

impl Default for TerminalRenderer {
  fn default() -> Self {
    Self::new(TerminalColor::Auto)
  }
}

impl ConsoleRenderer for TerminalRenderer {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    match entry.record() {
      ConsoleRecord::Execution(event) => self.render_execution_event(entry.timestamp(), event)?,
      ConsoleRecord::Diagnostic(diagnostic) => self.render_diagnostic(entry.timestamp(), diagnostic)?,
      ConsoleRecord::Document(document) => self.render_document(entry.timestamp(), document)?,
    }
    Ok(())
  }
}

impl TerminalRenderer {
  pub fn new(color: TerminalColor) -> Self {
    let styled = match color {
      TerminalColor::Auto => io::stdout().is_terminal(),
      TerminalColor::Always => true,
      TerminalColor::Never => false,
    };
    Self::with_writers(styled, Box::new(io::stdout()), Box::new(io::stderr()))
  }

  fn with_writers(styled: bool, stdout: Box<dyn Write + Send>, stderr: Box<dyn Write + Send>) -> Self {
    Self { styled, stdout, stderr }
  }

  fn render_execution_event(&mut self, timestamp: &DateTime<Utc>, event: &ExecutionEvent) -> io::Result<()> {
    match event {
      ExecutionEvent::RunStarted { command, .. } => self.write_log(
        timestamp,
        ConsoleLevel::Info,
        &format!("Starting execution plan for command {command}"),
      )?,
      ExecutionEvent::RunFinished {
        status: ConsoleStatus::Success,
        ..
      } => self.write_log(timestamp, ConsoleLevel::Info, "All tasks completed successfully")?,
      ExecutionEvent::RunFinished { .. }
      | ExecutionEvent::ScopeDeclared { .. }
      | ExecutionEvent::ScopeStarted { .. }
      | ExecutionEvent::ScopeFinished { .. } => {},
      ExecutionEvent::Output { stream, payload, .. } => match stream {
        ConsoleStream::Stdout => write_payload(&mut *self.stdout, payload)?,
        ConsoleStream::Stderr => write_payload(&mut *self.stderr, payload)?,
      },
    }
    Ok(())
  }

  fn render_diagnostic(&mut self, timestamp: &DateTime<Utc>, diagnostic: &ConsoleDiagnostic) -> io::Result<()> {
    self.write_log(timestamp, diagnostic.level, &diagnostic.message)
  }

  fn render_document(&mut self, timestamp: &DateTime<Utc>, document: &CliDocument) -> io::Result<()> {
    match document {
      CliDocument::TaskList { tasks } => {
        for task in tasks {
          let line = task.description.as_ref().map_or_else(
            || task.name.clone(),
            |description| format!("{}: {description}", task.name),
          );
          writeln!(self.stdout, "{line}")?;
        }
        self.stdout.flush()?;
      },
      CliDocument::Help { text } | CliDocument::Completion { text } => write_text(&mut *self.stdout, text)?,
      CliDocument::Failure { message } => {
        writeln!(self.stderr, "{message}")?;
        self.stderr.flush()?;
      },
      CliDocument::Summary { tasks, total } => {
        self.write_log(
          timestamp,
          ConsoleLevel::Info,
          "================== Time Summary ==================",
        )?;
        for item in tasks {
          self.write_log(
            timestamp,
            ConsoleLevel::Info,
            &format!(" \"{}\": {}", item.name, item.duration.human(Truncate::Millis)),
          )?;
        }
        self.write_log(
          timestamp,
          ConsoleLevel::Info,
          &format!(" Total time: {}", total.human(Truncate::Millis)),
        )?;
        self.write_log(
          timestamp,
          ConsoleLevel::Info,
          "==================================================",
        )?;
      },
    }
    Ok(())
  }

  fn write_log(&mut self, timestamp: &DateTime<Utc>, level: ConsoleLevel, message: &str) -> io::Result<()> {
    self
      .stdout
      .write_all(format_log_line(timestamp, self.styled, level, message).as_bytes())?;
    self.stdout.flush()
  }
}

fn format_log_line(timestamp: &DateTime<Utc>, styled: bool, level: ConsoleLevel, message: &str) -> String {
  if !styled {
    return format!("{message}\n");
  }

  let timestamp = Style::new()
    .fg(Color::DarkGray)
    .paint(timestamp.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string());
  let prefix = Style::new().on(Color::Blue).fg(Color::Yellow).paint("[octa]");
  let style = match level {
    ConsoleLevel::Trace => Style::new().fg(Color::Purple),
    ConsoleLevel::Debug => Style::new().fg(Color::Blue),
    ConsoleLevel::Info => Style::new().fg(Color::Green),
    ConsoleLevel::Warn => Style::new().fg(Color::Yellow),
    ConsoleLevel::Error => Style::new().fg(Color::Red),
  };
  format!("{timestamp} {prefix} {}\n", style.paint(message))
}

fn write_text(mut writer: impl Write, text: &str) -> io::Result<()> {
  writer.write_all(text.as_bytes())?;
  writer.flush()
}

fn write_payload(mut writer: impl Write, payload: &ConsolePayload) -> io::Result<()> {
  match payload {
    ConsolePayload::Line(line) => {
      writer.write_all(line.as_bytes())?;
      writer.write_all(b"\n")?;
      writer.flush()
    },
    ConsolePayload::Bytes(bytes) | ConsolePayload::RawBytes(bytes) => {
      writer.write_all(bytes)?;
      writer.flush()
    },
  }
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};

  use super::*;
  use crate::SummaryItem;

  fn entry(record: ConsoleRecord) -> ConsoleEntry {
    ConsoleEntry::new(record)
  }

  #[derive(Clone, Default)]
  struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

  impl Write for SharedBuffer {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
      self.0.lock().unwrap().write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  #[derive(Default)]
  struct FlushCountingWriter {
    bytes: Vec<u8>,
    flushes: usize,
  }

  impl Write for FlushCountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
      self.bytes.extend_from_slice(buffer);
      Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
      self.flushes += 1;
      Ok(())
    }
  }

  #[test]
  fn accepts_diagnostics_and_summary_documents() {
    let stdout = SharedBuffer::default();
    let stderr = SharedBuffer::default();
    let mut renderer = TerminalRenderer::with_writers(false, Box::new(stdout.clone()), Box::new(stderr.clone()));

    renderer
      .render(&entry(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        run_id: Some(7),
        scope: None,
        level: ConsoleLevel::Debug,
        message: "diagnostic".to_owned(),
        location: None,
      })))
      .unwrap();
    renderer
      .render(&entry(ConsoleRecord::Document(CliDocument::Summary {
        tasks: vec![SummaryItem {
          name: "build".to_owned(),
          duration: std::time::Duration::from_millis(25),
        }],
        total: std::time::Duration::from_millis(30),
      })))
      .unwrap();

    let output = String::from_utf8(stdout.0.lock().unwrap().clone()).unwrap();
    assert!(output.contains("diagnostic"));
    assert!(output.contains("build"));
    assert!(output.contains("Total time"));
    assert!(stderr.0.lock().unwrap().is_empty());
  }

  #[test]
  fn routes_documents_and_execution_output_to_their_streams() {
    let stdout = SharedBuffer::default();
    let stderr = SharedBuffer::default();
    let mut renderer = TerminalRenderer::with_writers(false, Box::new(stdout.clone()), Box::new(stderr.clone()));

    renderer
      .render(&entry(ConsoleRecord::Document(CliDocument::TaskList {
        tasks: vec![crate::TaskListItem {
          name: "build".to_owned(),
          description: Some("Build project".to_owned()),
        }],
      })))
      .unwrap();
    renderer
      .render(&entry(ConsoleRecord::Document(CliDocument::Help {
        text: "help\n".to_owned(),
      })))
      .unwrap();
    renderer
      .render(&entry(ConsoleRecord::Document(CliDocument::Failure {
        message: "failure".to_owned(),
      })))
      .unwrap();
    for (stream, line) in [(ConsoleStream::Stdout, "out"), (ConsoleStream::Stderr, "err")] {
      renderer
        .render(&entry(ConsoleRecord::Execution(ExecutionEvent::Output {
          run_id: 7,
          scope: None,
          command_id: "command-1".to_owned(),
          stream,
          payload: ConsolePayload::Line(line.to_owned()),
        })))
        .unwrap();
    }

    let stdout = String::from_utf8(stdout.0.lock().unwrap().clone()).unwrap();
    let stderr = String::from_utf8(stderr.0.lock().unwrap().clone()).unwrap();
    assert!(stdout.contains("build: Build project\nhelp\nout\n"));
    assert_eq!(stderr, "failure\nerr\n");
  }

  #[test]
  fn supports_explicit_terminal_color_policies() {
    assert!(TerminalRenderer::new(TerminalColor::Always).styled);
    assert!(!TerminalRenderer::new(TerminalColor::Never).styled);
  }

  #[test]
  fn payload_writer_flushes_line_and_raw_output() {
    let mut output = FlushCountingWriter::default();
    write_payload(&mut output, &ConsolePayload::Line("line".to_owned())).unwrap();
    write_payload(&mut output, &ConsolePayload::Bytes(b"raw".to_vec())).unwrap();
    assert_eq!(output.bytes, b"line\nraw");
    assert_eq!(output.flushes, 2);
  }

  #[test]
  fn text_writer_preserves_complete_cli_output() {
    let mut output = Vec::new();
    write_text(&mut output, "build: Build project\n").unwrap();
    assert_eq!(output, b"build: Build project\n");
  }

  #[test]
  fn log_lines_support_plain_and_styled_terminal_output() {
    let timestamp = Utc::now();
    assert_eq!(
      format_log_line(&timestamp, false, ConsoleLevel::Info, "message"),
      "message\n"
    );

    for level in [
      ConsoleLevel::Trace,
      ConsoleLevel::Debug,
      ConsoleLevel::Info,
      ConsoleLevel::Warn,
      ConsoleLevel::Error,
    ] {
      let line = format_log_line(&timestamp, true, level, "message");
      assert!(line.contains("[octa]"));
      assert!(line.contains("message"));
      assert!(line.ends_with('\n'));
    }
  }
}
