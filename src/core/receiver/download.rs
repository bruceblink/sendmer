/// 检查远端声明的文件数量和 payload 总字节数是否符合本地配额。
fn validate_receive_limits(
    total_files: u64,
    payload_size: u64,
    limits: ReceiveLimits,
) -> anyhow::Result<()> {
    if let Some(max_files) = limits.max_files {
        anyhow::ensure!(
            total_files <= max_files.get(),
            "received transfer contains {total_files} files, exceeding the local limit of {}",
            max_files
        );
    }
    if let Some(max_total_size_bytes) = limits.max_total_size_bytes {
        anyhow::ensure!(
            payload_size <= max_total_size_bytes.get(),
            "received transfer contains {payload_size} bytes, exceeding the local limit of {}",
            max_total_size_bytes
        );
    }
    Ok(())
}

/// 在每次重连前检查已保存的数据，防止恢复过程越过接收端配额。
fn validate_partial_receive_limits(
    local_bytes: u64,
    local_children: Option<u64>,
    plan: &DownloadPlan,
    limits: ReceiveLimits,
) -> anyhow::Result<()> {
    if let Some(max_files) = limits.max_files
        && let Some(children) = local_children
    {
        let files = completed_local_total_files(children);
        anyhow::ensure!(
            files <= max_files.get(),
            "received transfer already contains {files} files, exceeding the local limit of {}",
            max_files
        );
    }
    if let Some(max_total_size_bytes) = limits.max_total_size_bytes {
        let protocol_overhead = plan.transfer_total.saturating_sub(plan.payload_size);
        let max_transfer_bytes = max_total_size_bytes.get().saturating_add(protocol_overhead);
        anyhow::ensure!(
            local_bytes <= max_transfer_bytes,
            "received transfer already contains more than the local byte limit of {}",
            max_total_size_bytes
        );
    }
    Ok(())
}

/// 读取本地 collection 中每个 payload 的已验证大小。
async fn collection_payload_totals(
    db: &Store,
    collection: &Collection,
    limits: ReceiveLimits,
) -> anyhow::Result<(u64, u64)> {
    if let Some(max_files) = limits.max_files {
        anyhow::ensure!(
            collection.len() as u64 <= max_files.get(),
            "received collection contains more than the local file limit of {}",
            max_files
        );
    }
    let mut total_size = 0_u64;
    for (_name, hash) in collection.iter() {
        let status = db.status(*hash).await.map_err(anyhow::Error::from)?;
        let size = match status {
            BlobStatus::Complete { size } => size,
            BlobStatus::Partial { .. } => {
                anyhow::bail!("received collection payload {hash} is incomplete")
            }
            BlobStatus::NotFound => anyhow::bail!("received collection payload {hash} is missing"),
        };
        total_size = total_size
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("received collection payload size exceeds u64 range"))?;
        if let Some(max_total_size_bytes) = limits.max_total_size_bytes {
            anyhow::ensure!(
                total_size <= max_total_size_bytes.get(),
                "received collection exceeds the local byte limit of {}",
                max_total_size_bytes
            );
        }
    }
    Ok((collection.len() as u64, total_size))
}

async fn receive_once(
    context: &mut ReceiveContext,
    output_dir: &Path,
    event_emitter: TransferEventEmitter,
) -> anyhow::Result<ReceiveArtifacts> {
    tracing::trace!("load done!");
    download_missing_data(context, event_emitter.clone()).await?;
    let collection = context
        .load_collection()
        .await
        .context("load received collection")?;
    let manifest_hash = collection
        .iter()
        .find_map(|(name, hash)| (name == MANIFEST_COLLECTION_NAME).then_some(*hash));
    match manifest_hash {
        Some(hash) => receive_manifest(context, output_dir, event_emitter, collection, hash).await,
        None => receive_legacy_collection(context, output_dir, event_emitter, collection).await,
    }
}

async fn receive_manifest(
    context: &mut ReceiveContext,
    output_dir: &Path,
    event_emitter: TransferEventEmitter,
    collection: Collection,
    manifest_hash: iroh_blobs::Hash,
) -> anyhow::Result<ReceiveArtifacts> {
    let manifest_bytes = context.db.get_bytes(manifest_hash).await.map_err(|error| {
        receive_failure(
            error.into(),
            TransferErrorCode::TransferInterrupted,
            TransferPhase::Metadata,
            false,
            "received transfer manifest is unavailable",
        )
    })?;
    let manifest = TransferManifest::from_json_bytes(&manifest_bytes).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::RemoteRejected,
            TransferPhase::Metadata,
            false,
            "received transfer manifest is invalid",
        )
    })?;
    let file_names = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == ManifestEntryKind::File)
        .map(|entry| entry.path.components.join("/"))
        .collect::<Vec<_>>();
    if !file_names.is_empty() {
        event_emitter.emit_file_names(file_names);
    }
    let root_item_path = output_dir.join(manifest.root.path.to_path_buf()?);
    let file_count = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == ManifestEntryKind::File)
        .count() as u64;
    let payload_size = manifest.total_payload_size().map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::RemoteRejected,
            TransferPhase::Metadata,
            false,
            "received transfer manifest payload sizes are invalid",
        )
    })?;
    validate_receive_limits(file_count, payload_size, context.limits).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::RemoteRejected,
            TransferPhase::Metadata,
            false,
            "received transfer exceeds local resource limits",
        )
    })?;
    export_manifest_atomically(
        &context.db,
        &collection,
        &manifest,
        output_dir,
        &mut context.active_staging_dir,
    )
    .await
    .context("export transfer manifest")
    .map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Exporting,
            false,
            "unable to export transfer manifest",
        )
    })?;
    Ok(ReceiveArtifacts {
        total_files: file_count,
        payload_size,
        root_item_path,
    })
}

async fn receive_legacy_collection(
    context: &mut ReceiveContext,
    output_dir: &Path,
    event_emitter: TransferEventEmitter,
    collection: Collection,
) -> anyhow::Result<ReceiveArtifacts> {
    emit_collection_file_names(&event_emitter, &collection);
    let root_item_path = resolve_root_item_path(output_dir, &collection)
        .context("resolve received output path")
        .map_err(|error| {
            receive_failure(
                error,
                TransferErrorCode::RemoteRejected,
                TransferPhase::Metadata,
                false,
                "received collection contains unsupported paths",
            )
        })?;
    let (total_files, payload_size) = collection_payload_totals(
        &context.db,
        &collection,
        context.limits,
    )
    .await
    .context("inspect received collection payloads")
    .map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Metadata,
            false,
            "unable to inspect received file sizes",
        )
    })?;
    validate_receive_limits(total_files, payload_size, context.limits).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::RemoteRejected,
            TransferPhase::Metadata,
            false,
            "received transfer exceeds local resource limits",
        )
    })?;
    export_atomically(
        &context.db,
        collection,
        output_dir,
        &mut context.active_staging_dir,
    )
    .await
    .context("export received files")
    .map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Exporting,
            false,
            "unable to export received files",
        )
    })?;
    Ok(ReceiveArtifacts {
        total_files,
        payload_size,
        root_item_path,
    })
}

fn emit_collection_file_names(emitter: &TransferEventEmitter, collection: &Collection) {
    let file_names = collect_file_names(collection);
    if !file_names.is_empty() {
        emitter.emit_file_names(file_names);
    }
}

/// Format a user-facing failure while keeping both stage context and root cause.
fn receive_failed_message(error: &anyhow::Error) -> String {
    let details = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    format!("error: {details}")
}

fn emit_receive_failed(emitter: &TransferEventEmitter, error: &anyhow::Error) {
    if let Some(details) = classified_transfer_error(error) {
        emitter.emit_failed(details);
    } else {
        emitter.emit_internal_failure("receive failed");
    }
}

/// Pair an internal receive error with the stable details exposed to event consumers.
fn receive_failure(
    error: anyhow::Error,
    code: TransferErrorCode,
    phase: TransferPhase,
    retryable: bool,
    message: &'static str,
) -> anyhow::Error {
    classify_transfer_error(error, TransferError::new(code, phase, retryable, message))
}

/// Classify protocol failures without asking consumers to parse `GetError` text.
fn receive_protocol_failure(
    error: anyhow::Error,
    phase: TransferPhase,
    fallback_message: &'static str,
) -> anyhow::Error {
    let (code, retryable, message) = match error.downcast_ref::<GetError>() {
        Some(GetError::BadRequest { .. }) => (
            TransferErrorCode::RemoteRejected,
            false,
            "sender rejected the transfer request",
        ),
        Some(GetError::LocalFailure { .. }) => (
            TransferErrorCode::Internal,
            false,
            "local transfer processing failed",
        ),
        _ => (
            TransferErrorCode::TransferInterrupted,
            true,
            fallback_message,
        ),
    };
    receive_failure(error, code, phase, retryable, message)
}

fn finalize_failed_receive(
    primary_error: anyhow::Error,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Error {
    if let Err(error) = &cleanup_result {
        tracing::warn!(error = %error, "failed to cleanup receive context after error");
    }
    primary_error
}

async fn cleanup_failed_receive(context: &mut ReceiveContext) -> anyhow::Result<()> {
    let endpoint_cleanup_result = close_receive_endpoint(&context.endpoint).await;
    let store_cleanup_result =
        shutdown_receive_store_and_cleanup_staging(&context.db, &mut context.active_staging_dir)
            .await;
    let preserve_uncached_storage =
        should_preserve_uncached_storage(context.cache_lease.is_some(), &store_cleanup_result);
    let shutdown_result = combine_cleanup_results(endpoint_cleanup_result, store_cleanup_result);
    let storage_cleanup_result =
        finalize_receive_storage_after_store_cleanup(context, false, preserve_uncached_storage)
            .await;
    finalize_cleanup(shutdown_result, storage_cleanup_result)
}

const fn should_preserve_uncached_storage(
    cache_lease_present: bool,
    store_cleanup_result: &anyhow::Result<()>,
) -> bool {
    !cache_lease_present && store_cleanup_result.is_err()
}

async fn finish_receive(
    context: &mut ReceiveContext,
    artifacts: ReceiveArtifacts,
) -> anyhow::Result<ReceiveResult> {
    let endpoint_cleanup_result = close_receive_endpoint(&context.endpoint).await;
    let store_cleanup_result =
        shutdown_receive_store_and_cleanup_staging(&context.db, &mut context.active_staging_dir)
            .await;
    let preserve_uncached_storage =
        should_preserve_uncached_storage(context.cache_lease.is_some(), &store_cleanup_result);
    let shutdown_result = combine_cleanup_results(endpoint_cleanup_result, store_cleanup_result);
    let storage_cleanup_result = finalize_receive_storage_after_store_cleanup(
        context,
        shutdown_result.is_ok(),
        preserve_uncached_storage,
    )
    .await;
    finalize_cleanup(shutdown_result, storage_cleanup_result)?;

    Ok(ReceiveResult {
        message: format!(
            "Downloaded {} files, {} bytes",
            artifacts.total_files, artifacts.payload_size
        ),
        file_path: artifacts.root_item_path,
    })
}

/// Drain submitted export work, shut down the store, then remove its tracked staging tree.
///
/// iroh-blobs dispatches exports on its own runtime; `Store::shutdown` only forwards to its
/// metadata database. `wait_idle` is therefore required while an export owns a staging path,
/// and cleanup runs after both operations have completed.
async fn shutdown_receive_store_and_cleanup_staging(
    db: &Store,
    active_staging_dir: &mut Option<PathBuf>,
) -> anyhow::Result<()> {
    let idle_result = if active_staging_dir.is_some() {
        with_shutdown_deadline("receiver store wait_idle", async {
            db.wait_idle().await.map_err(anyhow::Error::from)
        })
        .await
    } else {
        Ok(())
    };
    let shutdown_result = with_shutdown_deadline("receiver store shutdown", async {
        db.shutdown().await.map_err(anyhow::Error::from)
    })
    .await;
    // 任一存储关闭步骤超时后，后台导出任务的所有权仍不确定。保留 staging
    // 目录，避免在任务仍访问文件时删除目录，后续由缓存清理流程处理残留。
    let staging_cleanup_result = if idle_result.is_ok() && shutdown_result.is_ok() {
        cleanup_active_staging_dir(active_staging_dir).await
    } else {
        tracing::warn!(
            error = ?idle_result.as_ref().err().or_else(|| shutdown_result.as_ref().err()),
            "skip staging cleanup after receiver store shutdown failure"
        );
        Ok(())
    };
    finalize_cleanup(
        combine_cleanup_results(idle_result, shutdown_result),
        staging_cleanup_result,
    )
}

/// Remove the active export staging directory after the blob store has stopped its export tasks.
async fn cleanup_active_staging_dir(
    active_staging_dir: &mut Option<PathBuf>,
) -> anyhow::Result<()> {
    let Some(path) = active_staging_dir.as_ref() else {
        return Ok(());
    };
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => {
            *active_staging_dir = None;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            *active_staging_dir = None;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Preserve persistent data after failure, but remove it after a clean export.
async fn finalize_receive_storage(
    context: &mut ReceiveContext,
    receive_succeeded: bool,
) -> anyhow::Result<()> {
    match context.cache_lease.take() {
        Some(lease) if receive_succeeded => lease.remove().await,
        Some(lease) => lease.preserve(),
        None => remove_temp_receive_dir(&context.iroh_data_dir).await,
    }
}

async fn finalize_receive_storage_after_store_cleanup(
    context: &mut ReceiveContext,
    receive_succeeded: bool,
    preserve_uncached_storage: bool,
) -> anyhow::Result<()> {
    if preserve_uncached_storage {
        // 存储关闭超时时保留无缓存目录，避免仍在运行的后台任务继续访问已删除的文件。
        tracing::warn!("preserve uncached receive storage after store shutdown failure");
        Ok(())
    } else {
        finalize_receive_storage(context, receive_succeeded).await
    }
}

/// Close the receive endpoint before its temporary store is finalized.
async fn close_receive_endpoint(endpoint: &Endpoint) -> anyhow::Result<()> {
    if endpoint.is_closed() {
        return Ok(());
    }
    with_shutdown_deadline("receive endpoint shutdown", async {
        endpoint.close().await;
        Ok(())
    })
    .await
}

async fn remove_temp_receive_dir(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
