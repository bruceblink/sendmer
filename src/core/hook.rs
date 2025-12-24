use async_trait::async_trait;
use iroh_blobs::provider::events::ProviderMessage;

#[async_trait]
pub trait SendHooks: Send + Sync {
    async fn on_import_start(&self, _files: usize) {}
    async fn on_import_progress(&self, _name: &str, _offset: u64, _total: u64) {}
    async fn on_import_done(&self) {}
    async fn on_provider_event(&self, _event: ProviderMessage) {}
}

#[async_trait]
pub trait ReceiveHooks: Send + Sync {
    async fn on_connecting(&self) {}
    async fn on_get_sizes(&self) {}
    async fn on_download_progress(&self, _offset: u64, _total: u64) {}
    async fn on_export_file(&self, _name: &str) {}
}
