use super::context::{DownloadPlan, ReceiveContext};
use super::errors::{receive_failure, receive_protocol_failure};
use super::limits::{
    completed_local_total_files_from_children, max_transfer_limit, validate_partial_receive_limits,
    validate_receive_limits,
};
use super::policy::{await_receive_phase, download_backoff};
use super::prepare::{get_sizes_with_retries, process_get_stream_with_limit};
use crate::core::events::{TransferErrorCode, TransferPhase};
use crate::core::progress::{ReceiverProgressReporter, TransferEventEmitter};
use anyhow::Context;

pub(super) async fn download_missing_data(
    context: &ReceiveContext,
    emitter: TransferEventEmitter,
) -> anyhow::Result<()> {
    let hash_and_format = context.hash_and_format();
    let local = context
        .db
        .remote()
        .local(hash_and_format)
        .await
        .map_err(|error| {
            receive_failure(
                error.into(),
                TransferErrorCode::Filesystem,
                TransferPhase::Transferring,
                false,
                "unable to inspect receiver storage",
            )
        })?;
    if local.is_complete() {
        completed_local_total_files_from_children(local.children())?;
        if let Some(lease) = &context.cache_lease {
            lease.checkpoint_recovery(&local, None);
        }
        return Ok(());
    }

    let (hash_seq, sizes) = get_sizes_with_retries(
        &context.endpoint,
        &context.addr,
        &context.ticket.hash(),
        context.retry_policy,
    )
    .await
    .context("fetch remote collection sizes")
    .map_err(|error| {
        receive_protocol_failure(
            error,
            TransferPhase::Metadata,
            "collection metadata transfer was interrupted",
        )
    })?;
    let plan = DownloadPlan::from_hash_seq_and_sizes(&hash_seq, &sizes)
        .context("validate collection metadata totals")?;
    validate_receive_limits(plan.total_files, plan.payload_size, context.limits).map_err(
        |error| {
            receive_failure(
                error,
                TransferErrorCode::RemoteRejected,
                TransferPhase::Metadata,
                false,
                "received transfer exceeds local resource limits",
            )
        },
    )?;
    if let Some(lease) = &context.cache_lease {
        lease.set_recovery_layout(sizes);
    }
    execute_download_with_retries(context, &plan, &emitter).await?;

    Ok(())
}

/// Reconnect and request only ranges still missing from the local store on each retry.
async fn execute_download_with_retries(
    context: &ReceiveContext,
    plan: &DownloadPlan,
    emitter: &TransferEventEmitter,
) -> anyhow::Result<()> {
    let mut last_error = None;
    let mut reporter = ReceiverProgressReporter::new(emitter.clone(), plan.transfer_total);
    reporter.emit_initial_progress();
    for attempt in 1..=context.retry_policy.download_retry_limit {
        let local = context
            .db
            .remote()
            .local(context.hash_and_format())
            .await
            .context("inspect local download state")
            .map_err(|error| {
                receive_failure(
                    error,
                    TransferErrorCode::Filesystem,
                    TransferPhase::Transferring,
                    false,
                    "unable to inspect receiver storage",
                )
            })?;
        if let Some(lease) = &context.cache_lease {
            lease.checkpoint_recovery(&local, None);
        }
        if local.is_complete() {
            reporter.emit_completed_progress();
            return Ok(());
        }
        validate_partial_receive_limits(
            local.local_bytes(),
            local.children(),
            plan,
            context.limits,
        )
        .map_err(|error| {
            receive_failure(
                error,
                TransferErrorCode::RemoteRejected,
                TransferPhase::Transferring,
                false,
                "received transfer exceeds local resource limits",
            )
        })?;

        let result = execute_download_attempt(
            context,
            local.missing(),
            local.local_bytes(),
            &mut reporter,
            plan,
        )
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(attempt, "blob download attempt failed");
                last_error = Some(error);
                if attempt < context.retry_policy.download_retry_limit {
                    tokio::time::sleep(download_backoff(attempt, context.retry_policy)).await;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        receive_failure(
            anyhow::anyhow!("download failed without an error"),
            TransferErrorCode::Internal,
            TransferPhase::Transferring,
            false,
            "download failed",
        )
    }))
}

/// Run one download attempt using a fresh connection and the currently missing ranges.
async fn execute_download_attempt(
    context: &ReceiveContext,
    missing: iroh_blobs::protocol::GetRequest,
    already_downloaded: u64,
    reporter: &mut ReceiverProgressReporter,
    plan: &DownloadPlan,
) -> anyhow::Result<()> {
    let connection = await_receive_phase(
        context.retry_policy.connect_timeout(),
        TransferPhase::Connecting,
        "connect to sender for blob download",
        async {
            context
                .endpoint
                .connect(context.addr.clone(), iroh_blobs::protocol::ALPN)
                .await
                .context("connect to sender for blob download")
        },
    )
    .await
    .map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::ConnectionFailed,
            TransferPhase::Connecting,
            true,
            "unable to connect to the sender",
        )
    })?;
    let get = context.db.remote().execute_get(connection, missing);
    let mut stream = get.stream();
    process_get_stream_with_limit(
        &mut stream,
        already_downloaded,
        reporter,
        context.retry_policy.download_idle_timeout(),
        max_transfer_limit(plan, context.limits),
        Some(context),
    )
    .await
    .context("download blob stream")
    .map_err(|error| {
        receive_protocol_failure(
            error,
            TransferPhase::Transferring,
            "data transfer was interrupted",
        )
    })
}
