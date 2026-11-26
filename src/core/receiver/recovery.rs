//! Refresh advisory checkpoints from iroh's verified local ranges, never telemetry.

use super::context::ReceiveContext;
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
            Ok(Ok(local)) => lease.checkpoint_recovery(&local, processed),
            _ => tracing::warn!("receive recovery observation unavailable; using blob store"),
        }
    }
}
