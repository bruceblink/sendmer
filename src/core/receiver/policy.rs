use super::errors::receive_failure;
use crate::core::events::{TransferErrorCode, TransferPhase};
use crate::core::options::ReceiveRetryPolicy;
use std::future::Future;
use std::time::Duration;

pub(super) fn size_fetch_backoff(
    attempt: u32,
    retry_policy: ReceiveRetryPolicy,
) -> std::time::Duration {
    ReceiveRetryPolicy::retry_backoff(retry_policy.size_fetch_backoff_ms, attempt)
}

pub(super) fn download_backoff(
    attempt: u32,
    retry_policy: ReceiveRetryPolicy,
) -> std::time::Duration {
    ReceiveRetryPolicy::retry_backoff(retry_policy.download_retry_backoff_ms, attempt)
}

/// Await one receive phase, applying an optional user-provided timeout.
///
/// The caller decides which retry loop handles the returned error, so timeouts
/// reuse the same cleanup and retry behavior as connection or protocol failures.
pub(super) async fn await_receive_phase<T, F>(
    timeout: Option<Duration>,
    transfer_phase: TransferPhase,
    phase: &'static str,
    future: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future).await.map_err(|_| {
            receive_failure(
                anyhow::anyhow!("{phase} timed out after {} ms", timeout.as_millis()),
                TransferErrorCode::Timeout,
                transfer_phase,
                true,
                timeout_event_message(transfer_phase),
            )
        })?,
        None => future.await,
    }
}

const fn timeout_event_message(phase: TransferPhase) -> &'static str {
    match phase {
        TransferPhase::Connecting => "connection timed out",
        TransferPhase::Metadata => "metadata request timed out",
        TransferPhase::Transferring => "download stalled without progress",
        TransferPhase::Preparing | TransferPhase::Exporting | TransferPhase::Finalizing => {
            "receive operation timed out"
        }
    }
}
