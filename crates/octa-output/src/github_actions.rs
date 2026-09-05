use std::io::{self, Write};

use super::{CliDocument, ConsoleEntry, ConsoleRecord, ConsoleRenderer};

/// Emits a GitHub Actions error annotation for a failed CLI invocation.
pub struct GithubActionsRenderer<R> {
  renderer: R,
  annotations: Box<dyn Write + Send>,
}

impl<R> GithubActionsRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self::with_writer(renderer, Box::new(io::stdout()))
  }

  fn with_writer(renderer: R, annotations: Box<dyn Write + Send>) -> Self {
    Self { renderer, annotations }
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for GithubActionsRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    let annotation_result = match entry.record() {
      ConsoleRecord::Document(CliDocument::Failure { message }) => self.write_error(message),
      _ => Ok(()),
    };
    let render_result = self.renderer.render(entry);

    annotation_result.and(render_result)
  }
}

impl<R> GithubActionsRenderer<R> {
  fn write_error(&mut self, message: &str) -> io::Result<()> {
    writeln!(
      self.annotations,
      "::error title={}::{}",
      escape_property("Octa failed"),
      escape_data(message)
    )?;
    self.annotations.flush()
  }
}

fn escape_data(value: &str) -> String {
  value.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A")
}

fn escape_property(value: &str) -> String {
  escape_data(value).replace(':', "%3A").replace(',', "%2C")
}

#[cfg(test)]
mod tests {
  use std::sync::{Arc, Mutex};

  use super::*;
  use crate::{ConsoleLevel, ConsoleRecord};

  #[derive(Clone, Default)]
  struct SharedBuffer(Arc<Mutex<Vec<u8>>>);

  impl Write for SharedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
      self.0.lock().unwrap().extend_from_slice(bytes);
      Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  #[derive(Default)]
  struct RecordingRenderer(Vec<ConsoleRecord>);

  impl ConsoleRenderer for RecordingRenderer {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.push(entry.record().clone());
      Ok(())
    }
  }

  struct FailingWriter;

  impl Write for FailingWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
      Err(io::Error::other("annotation failed"))
    }

    fn flush(&mut self) -> io::Result<()> {
      Ok(())
    }
  }

  #[test]
  fn annotates_final_failures_and_escapes_workflow_commands() {
    let annotations = SharedBuffer::default();
    let mut renderer = GithubActionsRenderer::with_writer(RecordingRenderer::default(), Box::new(annotations.clone()));
    let entry = ConsoleEntry::new(ConsoleRecord::Document(CliDocument::Failure {
      message: "bad%value\r\nnext".to_owned(),
    }));

    renderer.render(&entry).unwrap();

    assert_eq!(
      String::from_utf8(annotations.0.lock().unwrap().clone()).unwrap(),
      "::error title=Octa failed::bad%25value%0D%0Anext\n"
    );
    assert_eq!(renderer.renderer.0, vec![entry.record().clone()]);
  }

  #[test]
  fn ignores_diagnostics_and_successful_documents() {
    let annotations = SharedBuffer::default();
    let mut renderer = GithubActionsRenderer::with_writer(RecordingRenderer::default(), Box::new(annotations.clone()));
    let entry = ConsoleEntry::new(ConsoleRecord::Diagnostic(crate::ConsoleDiagnostic {
      run_id: None,
      scope: None,
      level: ConsoleLevel::Error,
      message: "intermediate error".to_owned(),
    }));

    renderer.render(&entry).unwrap();

    assert!(annotations.0.lock().unwrap().is_empty());
    assert_eq!(renderer.renderer.0, vec![entry.record().clone()]);
  }

  #[test]
  fn delegates_the_failure_even_when_the_annotation_sink_fails() {
    let mut renderer = GithubActionsRenderer::with_writer(RecordingRenderer::default(), Box::new(FailingWriter));
    let entry = ConsoleEntry::new(ConsoleRecord::Document(CliDocument::Failure {
      message: "failed".to_owned(),
    }));

    let error = renderer.render(&entry).unwrap_err();

    assert_eq!(error.to_string(), "annotation failed");
    assert_eq!(renderer.renderer.0, vec![entry.record().clone()]);
  }

  #[test]
  fn escapes_annotation_properties() {
    assert_eq!(escape_property("title: build, test%"), "title%3A build%2C test%25");
  }
}
