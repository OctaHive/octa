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
    let record = ConsoleRecord::Execution(ExecutionEvent::Output {
      run_id: 1,
      scope: None,
      command_id: "large".to_owned(),
      stream: ConsoleStream::Stdout,
      payload: ConsolePayload::Line("x".repeat(MEMORY_LIMIT + 1)),
    });
    let mut spool = EntrySpool::default();
    spool.push(ConsoleEntry::new(record.clone())).unwrap();
    assert!(matches!(&spool, EntrySpool::Disk(_)));

    let mut renderer = Recording::default();
    spool.render_into(&mut renderer).unwrap();
    assert_eq!(renderer.0, [record]);
  }
}
