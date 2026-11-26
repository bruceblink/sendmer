//! Minimal build: persistent cache operations fail before touching storage.

use super::ReceiveCachePruneReport;
use std::path::Path;

/// The signature remains available when the persistent cache is disabled.
pub async fn prune_receive_cache(
    _root: impl AsRef<Path>,
) -> anyhow::Result<ReceiveCachePruneReport> {
    std::future::ready(Err(anyhow::anyhow!(
        "persistent receive cache requires the persistent-recovery feature"
    )))
    .await
}
