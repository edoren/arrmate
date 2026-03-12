pub mod cleanup;
pub mod retry;

use anyhow::Result;
use async_trait::async_trait;
// use thiserror::Error;

// #[derive(Debug, Error)]
// enum TaskError {
//     #[error("task skipped: {0}")]
//     Skipped(String),

//     #[error("parse error: {0}")]
//     OK(#[from] std::num::ParseIntError),

//     #[error("io error: {0}")]
//     IoError(#[from] std::io::Error),
// }

#[async_trait]
pub trait Task: Send + Sync {
    fn name(&self) -> &str;
    async fn execute(&mut self) -> Result<()>;
}
