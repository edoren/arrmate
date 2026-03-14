pub mod cleanup;
pub mod retry;

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Task: Send + Sync {
    fn name(&self) -> &str;
    async fn execute(&mut self) -> Result<()>;
    fn next_date(&self, from: time::OffsetDateTime) -> Option<time::OffsetDateTime>;
}
