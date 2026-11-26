use super::context::ReceiveContext;
use super::errors::{receive_failure, receive_protocol_failure, show_get_error};
use super::policy::{await_receive_phase, size_fetch_backoff};
use crate::core::events::{TransferErrorCode, TransferPhase};
use crate::core::options::{MAX_LEGACY_COLLECTION_METADATA_BYTES, ReceiveRetryPolicy};
use crate::core::progress::ReceiverProgressReporter;
use anyhow::Context;
use iroh::Endpoint;
use iroh_blobs::api::remote::GetProgressItem;
use iroh_blobs::get::request::get_hash_seq_and_sizes;
use n0_future::StreamExt;
use std::sync::Arc as StdArc;
use std::time::Duration;
#[cfg(feature = "persistent-recovery")]
use std::time::Instant;

/// Fetch remote collection sizes with a fresh connection for each retry.
///
/// Reconnecting inside the attempt loop means an initial connection failure is
/// retried too, instead of skipping the retry policy before any request starts.
pub(super) async fn get_sizes_with_retries(
    endpoint: &Endpoint,
    addr: &iroh::EndpointAddr,
    hash: &iroh_blobs::Hash,
    retry_policy: ReceiveRetryPolicy,
) -> anyhow::Result<(iroh_blobs::hashseq::HashSeq, StdArc<[u64]>)> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=retry_policy.size_fetch_retry_limit {
        let connection = match await_receive_phase(
            retry_policy.connect_timeout(),
            TransferPhase::Connecting,
            "connect to sender for collection metadata",
            async {
                endpoint
                    .connect(addr.clone(), iroh_blobs::protocol::ALPN)
                    .await
                    .context("connect to sender for collection metadata")
            },
        )
        .await
        {
            Ok(connection) => connection,
            Err(error) => {
                let error = receive_failure(
                    error,
                    TransferErrorCode::ConnectionFailed,
                    TransferPhase::Connecting,
                    true,
                    "unable to connect to the sender",
                );
                tracing::error!(attempt, "collection metadata connection attempt failed");
                last_err = Some(error);
                if attempt < retry_policy.size_fetch_retry_limit {
                    tokio::time::sleep(size_fetch_backoff(attempt, retry_policy)).await;
                }
                continue;
            }
        };

        match await_receive_phase(
            retry_policy.metadata_timeout(),
            TransferPhase::Metadata,
            "fetch collection metadata",
            async {
                get_hash_seq_and_sizes(
                    &connection,
                    hash,
                    retry_policy
                        .size_fetch_chunk_size
                        .min(MAX_LEGACY_COLLECTION_METADATA_BYTES),
                    None,
                )
                .await
                .map_err(show_get_error)
                .map_err(anyhow::Error::from)
            },
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) => {
                let error = receive_protocol_failure(
                    error,
                    TransferPhase::Metadata,
                    "collection metadata transfer was interrupted",
                );
                tracing::error!(attempt, "collection metadata attempt failed");
                last_err = Some(error);
                if attempt < retry_policy.size_fetch_retry_limit {
                    tokio::time::sleep(size_fetch_backoff(attempt, retry_policy)).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        receive_failure(
            anyhow::anyhow!("unknown error getting sizes"),
            TransferErrorCode::Internal,
            TransferPhase::Metadata,
            false,
            "unable to fetch collection metadata",
        )
    }))
}

/// Consume one get stream without an additional byte ceiling.
#[cfg(test)]
pub(super) async fn process_get_stream_with_reporter<S>(
    stream: &mut S,
    already_downloaded: u64,
    reporter: &mut ReceiverProgressReporter,
    idle_timeout: Option<Duration>,
) -> anyhow::Result<()>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    process_get_stream_with_limit(
        stream,
        already_downloaded,
        reporter,
        idle_timeout,
        None,
        None,
    )
    .await
}

/// Consume one get stream and reject progress beyond the configured byte ceiling.
pub(super) async fn process_get_stream_with_limit<S>(
    stream: &mut S,
    already_downloaded: u64,
    reporter: &mut ReceiverProgressReporter,
    idle_timeout: Option<Duration>,
    max_transfer_bytes: Option<u64>,
    context: Option<&ReceiveContext>,
) -> anyhow::Result<()>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    let mut seen_done = false;
    #[cfg(not(feature = "persistent-recovery"))]
    let _ = context;
    #[cfg(feature = "persistent-recovery")]
    let mut last_checkpoint = Instant::now();
    while let Some(item) = next_get_progress_item(stream, idle_timeout).await? {
        match item {
            GetProgressItem::Progress(offset) => {
                tracing::trace!(offset, "received download progress");
                let downloaded = already_downloaded.saturating_add(offset);
                if let Some(max_transfer_bytes) = max_transfer_bytes {
                    anyhow::ensure!(
                        downloaded <= max_transfer_bytes,
                        "received transfer exceeds the configured byte limit"
                    );
                }
                #[cfg(feature = "persistent-recovery")]
                if let Some(context) = context
                    && let Some(cache_lease) = &context.cache_lease
                {
                    cache_lease.check_capacity()?;
                    if last_checkpoint.elapsed() >= Duration::from_secs(1) {
                        context.refresh_recovery(Some(downloaded)).await;
                        last_checkpoint = Instant::now();
                    }
                }
                reporter.on_progress(downloaded);
            }
            GetProgressItem::Done(value) => {
                let _stats = value;
                #[cfg(feature = "persistent-recovery")]
                if let Some(context) = context
                    && let Some(cache_lease) = &context.cache_lease
                {
                    cache_lease.check_capacity()?;
                    context.refresh_recovery(None).await;
                }
                reporter.emit_completed_progress();
                seen_done = true;
                break;
            }
            GetProgressItem::Error(cause) => {
                let error = show_get_error(cause);
                anyhow::bail!(error);
            }
        }
    }
    anyhow::ensure!(seen_done, "download stream ended before completion");
    Ok(())
}

/// Wait for the next stream update, restarting the idle timer after each update.
async fn next_get_progress_item<S>(
    stream: &mut S,
    idle_timeout: Option<Duration>,
) -> anyhow::Result<Option<GetProgressItem>>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    match idle_timeout {
        Some(timeout) => tokio::time::timeout(timeout, stream.next())
            .await
            .map_err(|_| {
                receive_failure(
                    anyhow::anyhow!(
                        "download stream timed out after {} ms without progress",
                        timeout.as_millis()
                    ),
                    TransferErrorCode::Timeout,
                    TransferPhase::Transferring,
                    true,
                    "download stalled without progress",
                )
            }),
        None => Ok(stream.next().await),
    }
}
