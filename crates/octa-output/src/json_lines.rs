use std::io::{self, Write};

use super::{ConsoleEntry, ConsoleRenderer};

/// Serializes each structured console entry as one JSON object per line.
pub struct JsonLinesRenderer {
  writer: Box<dyn Write + Send>,
}

impl Default for JsonLinesRenderer {
  fn default() -> Self {
    Self::new()
  }
}

impl JsonLinesRenderer {
  pub fn new() -> Self {
    Self::with_writer(Box::new(io::stdout()))
  }

  fn with_writer(writer: Box<dyn Write + Send>) -> Self {
    Self { writer }
  }
}

impl ConsoleRenderer for JsonLinesRenderer {
  fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
    serde_json::to_writer(&mut self.writer, entry).map_err(io::Error::other)?;
    self.writer.write_all(b"\n")?;
    self.writer.flush()
  }

  fn supports_raw_terminal(&self) -> bool {
    false
  }
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

  #[test]
  fn emits_one_complete_json_object_per_entry() {
    let output = SharedBuffer::default();
    let mut renderer = JsonLinesRenderer::with_writer(Box::new(output.clone()));
    let entry = ConsoleEntry::new(ConsoleRecord::Diagnostic(crate::ConsoleDiagnostic {
      run_id: Some(9),
      scope: None,
      level: ConsoleLevel::Info,
      message: "ready".to_owned(),
      location: None,
    }));

    renderer.render(&entry).unwrap();

    let bytes = output.0.lock().unwrap().clone();
    assert!(bytes.ends_with(b"\n"));
    let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["category"], "diagnostic");
    assert_eq!(value["data"]["message"], "ready");
  }

  #[test]
  fn default_renderer_is_machine_only() {
    let renderer = JsonLinesRenderer::default();
    assert!(!renderer.supports_raw_terminal());
  }
}
