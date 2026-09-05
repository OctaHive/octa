use std::{
  collections::HashMap,
  io::{self, Write},
};

use super::{
  CliDocument, ConsoleDiagnostic, ConsoleEntry, ConsoleLevel, ConsoleRecord, ConsoleRenderer, ConsoleScope,
  ConsoleStatus, ExecutionEvent, SourceLocation,
};

/// Emits a GitHub Actions error annotation for a failed CLI invocation.
pub struct GithubActionsRenderer<R> {
  renderer: R,
  annotations: Box<dyn Write + Send>,
  pending: HashMap<(u64, ConsoleScope), Vec<ConsoleDiagnostic>>,
  task_failure_emitted: bool,
}

impl<R> GithubActionsRenderer<R> {
  pub fn new(renderer: R) -> Self {
    Self::with_writer(renderer, Box::new(io::stdout()))
  }

  fn with_writer(renderer: R, annotations: Box<dyn Write + Send>) -> Self {
    Self {
      renderer,
      annotations,
      pending: HashMap::new(),
      task_failure_emitted: false,
    }
  }
}

impl<R: ConsoleRenderer> ConsoleRenderer for GithubActionsRenderer<R> {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    let annotation_result = match entry.record() {
      ConsoleRecord::Execution(ExecutionEvent::RunStarted { .. }) => {
        self.pending.clear();
        self.task_failure_emitted = false;
        Ok(())
      },
      ConsoleRecord::Diagnostic(diagnostic) if diagnostic.level == ConsoleLevel::Error => {
        if let (Some(run_id), Some(scope)) = (diagnostic.run_id, &diagnostic.scope) {
          self
            .pending
            .entry((run_id, scope.clone()))
            .or_default()
            .push(diagnostic.clone());
        }
        Ok(())
      },
      ConsoleRecord::Execution(ExecutionEvent::ScopeFinished { run_id, scope, status }) => {
        let diagnostics = self.pending.remove(&(*run_id, scope.clone())).unwrap_or_default();
        if *status == ConsoleStatus::Failed {
          if !diagnostics.is_empty() {
            self.task_failure_emitted = true;
            let mut first_error = None;
            for diagnostic in diagnostics {
              if let Err(error) = self.write_annotation(
                &format!("Task '{}' failed", scope.label()),
                &diagnostic.message,
                diagnostic.location.as_ref(),
              ) {
                first_error.get_or_insert(error);
              }
            }
            first_error.map_or(Ok(()), Err)
          } else {
            Ok(())
          }
        } else {
          Ok(())
        }
      },
      ConsoleRecord::Document(CliDocument::Failure { message }) if !self.task_failure_emitted => {
        self.write_error(message)
      },
      _ => Ok(()),
    };
    let render_result = self.renderer.render(entry);

    annotation_result.and(render_result)
  }

  fn supports_raw_terminal(&self) -> bool {
    self.renderer.supports_raw_terminal()
  }

  fn tick(&mut self) -> io::Result<()> {
    self.renderer.tick()
  }

  fn set_parallel(&mut self, parallel: bool) -> io::Result<()> {
    self.renderer.set_parallel(parallel)
  }

  fn begin_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.begin_raw(scope)
  }

  fn end_raw(&mut self, scope: &ConsoleScope) -> io::Result<()> {
    self.renderer.end_raw(scope)
  }
}

impl<R> GithubActionsRenderer<R> {
  fn write_error(&mut self, message: &str) -> io::Result<()> {
    self.write_annotation("Octa failed", message, None)
  }

  fn write_annotation(&mut self, title: &str, message: &str, location: Option<&SourceLocation>) -> io::Result<()> {
    let mut properties = format!("title={}", escape_property(title));
    if let Some(file) = location.map(|location| &location.file) {
      properties.push_str(&format!(",file={}", escape_property(file)));
    }
    if let Some(line) = location.and_then(|location| location.line) {
      properties.push_str(&format!(",line={line}"));
    }
    if let Some(column) = location.and_then(|location| location.column) {
      properties.push_str(&format!(",col={column}"));
    }
    writeln!(self.annotations, "::error {properties}::{}", escape_data(message))?;
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
  fn ignores_non_error_diagnostics_and_successful_documents() {
    let annotations = SharedBuffer::default();
    let mut renderer = GithubActionsRenderer::with_writer(RecordingRenderer::default(), Box::new(annotations.clone()));
    let entry = ConsoleEntry::new(ConsoleRecord::Diagnostic(crate::ConsoleDiagnostic {
      run_id: None,
      scope: None,
      level: ConsoleLevel::Info,
      message: "intermediate error".to_owned(),
      location: None,
    }));

    renderer.render(&entry).unwrap();

    assert!(annotations.0.lock().unwrap().is_empty());
    assert_eq!(renderer.renderer.0, vec![entry.record().clone()]);
  }

  #[test]
  fn annotates_task_errors_with_available_source_coordinates() {
    let annotations = SharedBuffer::default();
    let scope = crate::ConsoleScopeAllocator::default().scope("lint");
    let mut renderer = GithubActionsRenderer::with_writer(RecordingRenderer::default(), Box::new(annotations.clone()));
    let entry = ConsoleEntry::new(ConsoleRecord::Diagnostic(crate::ConsoleDiagnostic {
      run_id: Some(4),
      scope: Some(scope.clone()),
      level: ConsoleLevel::Error,
      message: "Octafile /work/Octafile.yml parsing error: bad value at line 12, column 7".to_owned(),
      location: Some(crate::SourceLocation {
        file: "/work/Octafile.yml".to_owned(),
        line: Some(12),
        column: Some(7),
      }),
    }));

    renderer.render(&entry).unwrap();
    assert!(annotations.0.lock().unwrap().is_empty());
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 4,
          scope,
          status: ConsoleStatus::Failed,
        },
      )))
      .unwrap();

    assert_eq!(
      String::from_utf8(annotations.0.lock().unwrap().clone()).unwrap(),
      "::error title=Task 'lint' failed,file=/work/Octafile.yml,line=12,col=7::Octafile /work/Octafile.yml parsing error%3A bad value at line 12%2C column 7\n"
        .replace("%3A", ":")
        .replace("%2C", ",")
    );
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
  fn successful_scope_discards_pending_error_annotation() {
    let annotations = SharedBuffer::default();
    let scope = crate::ConsoleScopeAllocator::default().scope("ignored");
    let mut renderer = GithubActionsRenderer::with_writer(RecordingRenderer::default(), Box::new(annotations.clone()));
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Diagnostic(
        crate::ConsoleDiagnostic {
          run_id: Some(1),
          scope: Some(scope.clone()),
          level: ConsoleLevel::Error,
          message: "ignored error".to_owned(),
          location: None,
        },
      )))
      .unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope,
          status: ConsoleStatus::Success,
        },
      )))
      .unwrap();
    assert!(annotations.0.lock().unwrap().is_empty());
  }

  #[test]
  fn final_failure_does_not_duplicate_a_task_annotation() {
    let annotations = SharedBuffer::default();
    let scope = crate::ConsoleScopeAllocator::default().scope("build");
    let mut renderer = GithubActionsRenderer::with_writer(RecordingRenderer::default(), Box::new(annotations.clone()));
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Diagnostic(
        crate::ConsoleDiagnostic {
          run_id: Some(1),
          scope: Some(scope.clone()),
          level: ConsoleLevel::Error,
          message: "failed".to_owned(),
          location: None,
        },
      )))
      .unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope,
          status: ConsoleStatus::Failed,
        },
      )))
      .unwrap();
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Document(CliDocument::Failure {
        message: "failed".to_owned(),
      })))
      .unwrap();
    let output = String::from_utf8(annotations.0.lock().unwrap().clone()).unwrap();
    assert_eq!(output.lines().count(), 1);
  }

  #[test]
  fn emits_every_error_diagnostic_attached_to_a_failed_task() {
    let annotations = SharedBuffer::default();
    let scope = crate::ConsoleScopeAllocator::default().scope("check");
    let mut renderer = GithubActionsRenderer::with_writer(RecordingRenderer::default(), Box::new(annotations.clone()));
    for message in ["first", "second"] {
      renderer
        .render(&ConsoleEntry::new(ConsoleRecord::Diagnostic(ConsoleDiagnostic {
          run_id: Some(1),
          scope: Some(scope.clone()),
          level: ConsoleLevel::Error,
          message: message.to_owned(),
          location: None,
        })))
        .unwrap();
    }
    renderer
      .render(&ConsoleEntry::new(ConsoleRecord::Execution(
        ExecutionEvent::ScopeFinished {
          run_id: 1,
          scope,
          status: ConsoleStatus::Failed,
        },
      )))
      .unwrap();

    let output = String::from_utf8(annotations.0.lock().unwrap().clone()).unwrap();
    assert_eq!(output.lines().count(), 2);
    assert!(output.contains("::first"));
    assert!(output.contains("::second"));
  }

  #[test]
  fn escapes_annotation_properties() {
    assert_eq!(escape_property("title: build, test%"), "title%3A build%2C test%25");
  }
}
