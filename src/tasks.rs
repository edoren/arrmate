pub mod cleanup;
pub mod retry;

use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait Task: Send {
    fn name(&self) -> &str;
    async fn execute(&mut self) -> Result<()>;
}
