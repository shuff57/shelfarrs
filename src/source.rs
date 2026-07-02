//! Host-side acquisition seam. The types live in `shelfarrs-sdk` (shared with
//! plugins); this trait is what the job runner calls, implemented by `WasmSource`.

use anyhow::Result;
use async_trait::async_trait;
pub use shelfarrs_sdk::{Candidate, Download, SearchQuery};

#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &str;
    async fn search(&self, q: &SearchQuery) -> Result<Vec<Candidate>>;
    async fn resolve_download(&self, reference: &str) -> Result<Download>;
}
