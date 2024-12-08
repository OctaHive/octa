use thiserror::Error;

pub type OctafileResult<T> = Result<T, OctafileError>;

#[derive(Error, Debug)]
pub enum OctafileError {
  #[error("Octafile not found traversing to root directory")]
  NotSearchedError,

  #[error("Octafile not found in path {0}")]
  NotFoundError(String),

  #[error("Octafile {0} parsing error: {1}")]
  ParseError(String, String),

  #[error("IO error when opening octafile: {0}")]
  IoError(#[from] std::io::Error),

  #[error("Failed to read octafile: {0}")]
  ReadError(String),

  #[error("Lock error: {0}")]
  LockError(String),
}
