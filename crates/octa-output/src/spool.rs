use std::{
  io::{self, BufRead, BufReader, BufWriter, Seek, SeekFrom, Write},
  mem,
};

use tempfile::NamedTempFile;

use super::{ConsoleEntry, ConsolePayload, ConsoleRecord, ConsoleRenderer, ExecutionEvent};

const MEMORY_LIMIT: usize = 1024 * 1024;

/// A bounded in-memory record buffer that transparently spills large task output to disk.
pub(crate) enum EntrySpool {
  Memory { entries: Vec<ConsoleEntry>, bytes: usize },
  Disk(BufWriter<NamedTempFile>),
}

impl Default for EntrySpool {
  fn default() -> Self {
    Self::Memory {
      entries: Vec::new(),
      bytes: 0,
    }
  }
}

impl EntrySpool {
  pub(crate) fn push(&mut self, entry: ConsoleEntry) -> io::Result<()> {
    let entry_bytes = estimated_memory(&entry);
    match self {
      Self::Memory { entries, bytes } if bytes.saturating_add(entry_bytes) <= MEMORY_LIMIT => {
        *bytes += entry_bytes;
        entries.push(entry);
        Ok(())
      },
      Self::Memory { entries, .. } => {
        let mut file = BufWriter::new(NamedTempFile::new()?);
        for buffered in entries.drain(..) {
          serde_json::to_writer(&mut file, &buffered).map_err(io::Error::other)?;
          file.write_all(b"\n")?;
        }
        serde_json::to_writer(&mut file, &entry).map_err(io::Error::other)?;
        file.write_all(b"\n")?;
        *self = Self::Disk(file);
        Ok(())
      },
      Self::Disk(file) => {
        serde_json::to_writer(&mut *file, &entry).map_err(io::Error::other)?;
        file.write_all(b"\n")
      },
    }
  }

  pub(crate) fn render_into(mut self, renderer: &mut impl ConsoleRenderer) -> io::Result<()> {
    match &mut self {
      Self::Memory { entries, .. } => render_entries(renderer, entries.drain(..)),
      Self::Disk(file) => {
        file.flush()?;
        file.get_mut().as_file_mut().seek(SeekFrom::Start(0))?;
        let reader = BufReader::new(file.get_mut().as_file_mut());
        let entries = reader.lines().map(|line| {
          let line = line?;
          serde_json::from_str::<ConsoleEntry>(&line).map_err(io::Error::other)
        });
        render_results(renderer, entries)
      },
    }
  }
}

fn estimated_memory(entry: &ConsoleEntry) -> usize {
  let dynamic = match entry.record() {
    ConsoleRecord::Execution(event) => match event {
      ExecutionEvent::RunStarted { command, .. } | ExecutionEvent::RunFinished { command, .. } => command.len(),
      ExecutionEvent::ScopeDeclared { scope, .. }
      | ExecutionEvent::ScopeStarted { scope, .. }
      | ExecutionEvent::ScopeFinished { scope, .. } => scope.label().len(),
      ExecutionEvent::StepDeclared { scope, step, .. }
      | ExecutionEvent::StepStarted { scope, step, .. }
      | ExecutionEvent::StepFinished { scope, step, .. } => scope.label().len() + step.label().len(),
      ExecutionEvent::Output {
        scope,
        command_id,
        payload,
        ..
      } => {
        let payload = match payload {
          ConsolePayload::Line(line) => line.len(),
          ConsolePayload::Bytes(bytes) | ConsolePayload::RawBytes(bytes) => bytes.len(),
        };
        scope.as_ref().map_or(0, |scope| scope.label().len()) + command_id.len() + payload
      },
      ExecutionEvent::Progress {
        scope,
        command_id,
        progress,
        ..
      } => {
        scope.as_ref().map_or(0, |scope| scope.label().len())
          + command_id.len()
          + progress.message.len()
          + progress.unit.as_ref().map_or(0, String::len)
      },
    },
    ConsoleRecord::Diagnostic(diagnostic) => {
      diagnostic.message.len()
        + diagnostic.scope.as_ref().map_or(0, |scope| scope.label().len())
        + diagnostic.location.as_ref().map_or(0, |location| location.file.len())
    },
    // Documents are unscoped and never enter a buffering renderer.
    ConsoleRecord::Document(_) => 0,
  };
  mem::size_of::<ConsoleEntry>().saturating_add(dynamic)
}

fn render_entries(renderer: &mut impl ConsoleRenderer, entries: impl Iterator<Item = ConsoleEntry>) -> io::Result<()> {
  render_results(renderer, entries.map(Ok))
}

fn render_results(
  renderer: &mut impl ConsoleRenderer,
  entries: impl Iterator<Item = io::Result<ConsoleEntry>>,
) -> io::Result<()> {
  let mut first_error = None;
  for entry in entries {
    let result = entry.and_then(|entry| renderer.render(&entry));
    if let Err(error) = result {
      if first_error.is_none() {
        first_error = Some(error);
      }
    }
  }
  first_error.map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::{ConsolePayload, ConsoleRecord, ConsoleStream, ExecutionEvent};

  #[derive(Default)]
  struct Recording(Vec<ConsoleRecord>);

  impl ConsoleRenderer for Recording {
    fn render(&mut self, entry: &ConsoleEntry) -> io::Result<()> {
      self.0.push(entry.record().clone());
      Ok(())
    }
  }

  #[test]
  fn spills_large_output_and_replays_it_losslessly() {
    let small = ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: None,
      step_id: None,
      command_id: "small".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Bytes(vec![1, 2, 3]),
    });
    let large = ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: None,
      step_id: None,
      command_id: "large".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Line("x".repeat(MEMORY_LIMIT + 1)),
    });
    let tail = ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: None,
      step_id: None,
      command_id: "tail".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::RawBytes(vec![4, 5, 6]),
    });
    let mut spool = EntrySpool::default();
    spool.push(ConsoleEntry::new(small.clone())).unwrap();
    spool.push(ConsoleEntry::new(large.clone())).unwrap();
    assert!(matches!(&spool, EntrySpool::Disk(_)));
    spool.push(ConsoleEntry::new(tail.clone())).unwrap();

    let mut renderer = Recording::default();
    spool.render_into(&mut renderer).unwrap();
    assert_eq!(renderer.0, [small, large, tail]);
  }

  #[test]
  fn estimates_dynamic_memory_for_each_record_shape() {
    use crate::{CliDocument, ConsoleDiagnostic, ConsoleLevel, ConsoleScopeAllocator, ConsoleStatus, ProgressUpdate};

    let scope = ConsoleScopeAllocator::default().scope("scope");
    let records = [
      ConsoleRecord::Execution(ExecutionEvent::RunStarted {
        run_id: 1,
        command: "command".to_owned(),
      }),
      ConsoleRecord::Execution(ExecutionEvent::RunFinished {
        run_id: 1,
        command: "command".to_owned(),
        status: ConsoleStatus::Success,
      }),
      ConsoleRecord::Execution(ExecutionEvent::ScopeStarted {
        run_id: 1,
        scope: scope.clone(),
      }),
      ConsoleRecord::Execution(ExecutionEvent::Progress {
        run_id: 1,
        scope: Some(scope.clone()),
        step_id: None,
        command_id: "command".to_owned(),
        progress: ProgressUpdate {
          message: "working".to_owned(),
          current: Some(1),
          total: Some(2),
          unit: Some("files".to_owned()),
        },
      }),
      ConsoleRecord::Diagnostic(ConsoleDiagnostic {
        run_id: Some(1),
        scope: Some(scope),
        step_id: None,
        level: ConsoleLevel::Warn,
        message: "warning".to_owned(),
        location: None,
      }),
      ConsoleRecord::Document(CliDocument::Help {
        text: "document".to_owned(),
      }),
    ];

    for record in records {
      assert!(estimated_memory(&ConsoleEntry::new(record)) >= mem::size_of::<ConsoleEntry>());
    }
  }
}
