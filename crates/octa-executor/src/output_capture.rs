use std::io::{self, SeekFrom};

use octa_output::ConsoleStream;
use tempfile::NamedTempFile;
use tokio::{
  fs::File,
  io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufWriter},
};

pub(crate) const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const CAPTURE_MEMORY_LIMIT: usize = 1024 * 1024;

#[derive(Debug)]
pub(crate) enum CaptureError {
  LimitExceeded,
  Io(io::Error),
}

impl From<io::Error> for CaptureError {
  fn from(error: io::Error) -> Self {
    Self::Io(error)
  }
}

enum CaptureBuffer {
  Memory(Vec<u8>),
  Disk {
    file: BufWriter<File>,
    _temporary: NamedTempFile,
  },
}

impl Default for CaptureBuffer {
  fn default() -> Self {
    Self::Memory(Vec::new())
  }
}

impl CaptureBuffer {
  async fn append(&mut self, bytes: &[u8], memory_limit: usize) -> io::Result<()> {
    match self {
      Self::Memory(buffer) if buffer.len().saturating_add(bytes.len()) <= memory_limit => {
        buffer.extend_from_slice(bytes);
        Ok(())
      },
      Self::Memory(buffer) => {
        let temporary = NamedTempFile::new()?;
        let mut file = BufWriter::new(File::from_std(temporary.reopen()?));
        file.write_all(buffer).await?;
        file.write_all(bytes).await?;
        *self = Self::Disk {
          file,
          _temporary: temporary,
        };
        Ok(())
      },
      Self::Disk { file, .. } => file.write_all(bytes).await,
    }
  }

  async fn into_string(self) -> io::Result<String> {
    match self {
      Self::Memory(buffer) => Ok(decode_output(buffer)),
      Self::Disk { mut file, .. } => {
        file.flush().await?;
        file.get_mut().seek(SeekFrom::Start(0)).await?;
        let mut output = Vec::new();
        file.get_mut().read_to_end(&mut output).await?;
        Ok(decode_output(output))
      },
    }
  }
}

fn decode_output(bytes: Vec<u8>) -> String {
  String::from_utf8(bytes).unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

/// Captures the task result with a shared stdout/stderr byte budget.
pub(crate) struct OutputCapture {
  stdout: CaptureBuffer,
  stderr: CaptureBuffer,
  bytes: usize,
  memory_limit: usize,
  max_bytes: usize,
}

impl Default for OutputCapture {
  fn default() -> Self {
    Self::with_limits(CAPTURE_MEMORY_LIMIT, MAX_CAPTURED_OUTPUT_BYTES)
  }
}

impl OutputCapture {
  fn with_limits(memory_limit: usize, max_bytes: usize) -> Self {
    Self {
      stdout: CaptureBuffer::default(),
      stderr: CaptureBuffer::default(),
      bytes: 0,
      memory_limit,
      max_bytes,
    }
  }

  pub(crate) async fn append(&mut self, stream: ConsoleStream, bytes: &[u8]) -> Result<(), CaptureError> {
    self.append_parts(stream, &[bytes]).await
  }

  pub(crate) async fn append_line(&mut self, stream: ConsoleStream, line: &str) -> Result<(), CaptureError> {
    self.append_parts(stream, &[line.as_bytes(), b"\n"]).await
  }

  async fn append_parts(&mut self, stream: ConsoleStream, parts: &[&[u8]]) -> Result<(), CaptureError> {
    let added = parts
      .iter()
      .try_fold(0usize, |total, part| total.checked_add(part.len()))
      .ok_or(CaptureError::LimitExceeded)?;
    let next = self.bytes.checked_add(added).ok_or(CaptureError::LimitExceeded)?;
    if next > self.max_bytes {
      return Err(CaptureError::LimitExceeded);
    }
    let buffer = match stream {
      ConsoleStream::Stdout => &mut self.stdout,
      ConsoleStream::Stderr => &mut self.stderr,
    };
    for part in parts {
      buffer.append(part, self.memory_limit).await?;
    }
    self.bytes = next;
    Ok(())
  }

  pub(crate) async fn into_strings(self) -> io::Result<(String, String)> {
    Ok((self.stdout.into_string().await?, self.stderr.into_string().await?))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn spills_each_stream_and_replays_utf8_losslessly() {
    let mut capture = OutputCapture::with_limits(4, 64);
    capture.append(ConsoleStream::Stdout, b"hello").await.unwrap();
    capture.append(ConsoleStream::Stderr, b"error").await.unwrap();
    assert!(matches!(capture.stdout, CaptureBuffer::Disk { .. }));
    assert!(matches!(capture.stderr, CaptureBuffer::Disk { .. }));

    let (stdout, stderr) = capture.into_strings().await.unwrap();
    assert_eq!(stdout, "hello");
    assert_eq!(stderr, "error");
  }

  #[tokio::test]
  async fn reassembles_utf8_split_across_byte_chunks() {
    let mut capture = OutputCapture::with_limits(64, 64);
    let value = "сборка".as_bytes();
    capture.append(ConsoleStream::Stdout, &value[..1]).await.unwrap();
    capture.append(ConsoleStream::Stdout, &value[1..]).await.unwrap();

    let (stdout, stderr) = capture.into_strings().await.unwrap();
    assert_eq!(stdout, "сборка");
    assert!(stderr.is_empty());
  }

  #[tokio::test]
  async fn replaces_invalid_bytes_only_after_reassembling_the_stream() {
    let mut capture = OutputCapture::with_limits(64, 64);
    capture.append(ConsoleStream::Stdout, &[0xff]).await.unwrap();

    let (stdout, _) = capture.into_strings().await.unwrap();
    assert_eq!(stdout, "�");
  }

  #[tokio::test]
  async fn enforces_one_limit_across_both_streams() {
    let mut capture = OutputCapture::with_limits(4, 9);
    capture.append(ConsoleStream::Stdout, b"hello").await.unwrap();
    capture.append(ConsoleStream::Stderr, b"four").await.unwrap();
    assert!(matches!(
      capture.append(ConsoleStream::Stdout, b"!").await,
      Err(CaptureError::LimitExceeded)
    ));
  }
}
