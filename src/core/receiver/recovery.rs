//! Refresh advisory checkpoints from iroh's verified local ranges, never telemetry.

use super::context::ReceiveContext;
use iroh_blobs::api::remote::LocalInfo;
use std::time::Duration;

impl ReceiveContext {
    /// Bound metadata observation time. Recovery bookkeeping must not become
    /// a new unbounded dependency of network progress or cleanup.
    pub(super) async fn refresh_recovery(&self, processed: Option<u64>) {
        let Some(lease) = &self.cache_lease else {
            return;
        };
        if !lease.recovery_enabled() {
            return;
        }
        match tokio::time::timeout(
            Duration::from_secs(2),
            self.db.remote().local(self.hash_and_format()),
        )
        .await
        {
            Ok(Ok(local)) => self.checkpoint_recovery(&local, processed).await,
            _ => tracing::warn!("receive recovery observation unavailable; using blob store"),
        }
    }

    /// Commit submitted iroh metadata writes before publishing a live range
    /// summary. A process can otherwise die while the metadata transaction
    /// that references its verified blob files is still open.
    pub(super) async fn checkpoint_recovery(&self, local: &LocalInfo, processed: Option<u64>) {
        let Some(lease) = &self.cache_lease else {
            return;
        };
        if !lease.recovery_enabled() {
            return;
        }
        match tokio::time::timeout(Duration::from_secs(2), self.db.sync_db()).await {
            Ok(Ok(())) => lease.checkpoint_recovery(local, processed),
            _ => tracing::warn!("receive recovery metadata sync unavailable; using blob store"),
        }
    }
}
