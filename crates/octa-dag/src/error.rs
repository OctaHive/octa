use thiserror::Error;

pub type DAGResult<T> = Result<T, DAGError>;

#[derive(Error, Debug)]
pub enum DAGError {
  #[error("Node not found: {0}")]
  NodeNotFound(String),
}
