async fn download_missing_data(
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
    execute_download_with_retries(context, &plan, &emitter).await?;

    Ok(())
}

const fn completed_local_total_files(children: u64) -> u64 {
    children.saturating_sub(1)
}

fn completed_local_total_files_from_children(children: Option<u64>) -> anyhow::Result<u64> {
    children
        .map(completed_local_total_files)
        .ok_or_else(|| anyhow::anyhow!("local complete state missing collection children"))
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
        context.cache_lease.as_ref(),
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

fn collect_file_names(collection: &Collection) -> Vec<String> {
    collection
        .iter()
        .map(|(name, _hash)| name.to_string())
        .collect()
}

fn resolve_root_item_path(output_dir: &Path, collection: &Collection) -> anyhow::Result<PathBuf> {
    let root = collection_root_name(collection)?;
    Ok(output_dir.join(root))
}

fn size_fetch_backoff(attempt: u32, retry_policy: ReceiveRetryPolicy) -> std::time::Duration {
    ReceiveRetryPolicy::retry_backoff(retry_policy.size_fetch_backoff_ms, attempt)
}

fn download_backoff(attempt: u32, retry_policy: ReceiveRetryPolicy) -> std::time::Duration {
    ReceiveRetryPolicy::retry_backoff(retry_policy.download_retry_backoff_ms, attempt)
}

/// Await one receive phase, applying an optional user-provided timeout.
///
/// The caller decides which retry loop handles the returned error, so timeouts
/// reuse the same cleanup and retry behavior as connection or protocol failures.
async fn await_receive_phase<T, F>(
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

fn finalize_cleanup(
    shutdown_result: anyhow::Result<()>,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    if let Err(error) = &cleanup_result {
        tracing::warn!(error = %error, "failed to clean temporary receive dir");
    }
    shutdown_result
}

/// 合并有序关闭步骤的结果，保留第一个失败步骤及其错误链。
fn combine_cleanup_results(
    first: anyhow::Result<()>,
    second: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first_error), Err(second_error)) => Err(first_error.context(format!(
            "subsequent cleanup step also failed: {second_error:#}"
        ))),
    }
}

/// 记录 `GetError` 的稳定类别并原样返回，便于上层保留结构化失败语义。
fn show_get_error(e: GetError) -> GetError {
    log_get_error(&e);
    e
}

fn log_get_error(e: &GetError) {
    match e {
        GetError::InitialNext { .. }
        | GetError::ConnectedNext { .. }
        | GetError::AtBlobHeaderNext { .. } => {
            log_get_error_connection(e);
        }
        GetError::Decode { .. } | GetError::IrpcSend { .. } => {
            log_get_error_decode_or_irpc(e);
        }
        GetError::AtClosingNext { .. }
        | GetError::BadRequest { .. }
        | GetError::LocalFailure { .. } => {
            log_get_error_misc(e);
        }
    }
}

fn log_get_error_connection(e: &GetError) {
    match e {
        GetError::InitialNext { .. } => tracing::error!("initial connection error"),
        GetError::ConnectedNext { .. } => tracing::error!("connected error"),
        GetError::AtBlobHeaderNext { .. } => tracing::error!("reading blob header error"),
        _ => {}
    }
}

fn log_get_error_decode_or_irpc(e: &GetError) {
    match e {
        GetError::Decode { .. } => tracing::error!("decoding error"),
        GetError::IrpcSend { .. } => tracing::error!("error sending over irpc"),
        _ => {}
    }
}

fn log_get_error_misc(e: &GetError) {
    match e {
        GetError::AtClosingNext { .. } => tracing::error!("error at closing"),
        GetError::BadRequest { .. } => tracing::error!("bad request"),
        GetError::LocalFailure { .. } => tracing::error!("local failure"),
        _ => {}
    }
}

// Helper: prepare endpoint, temp dir and FsStore
