/// 开始共享（发送）指定的 `path`（文件或目录）。
///
/// - `path`：要分享的文件或目录路径。
/// - `options`：发送配置（转发模式、ticket 类型等）。
/// - `app_handle`：可选的事件发射器句柄，用于 UI/CLI 上报进度。
///
/// 返回 `SendResult`，其中包含票据、hash、大小以及需要保持存活的 router/store 句柄。
pub async fn send(
    path: PathBuf,
    options: SendOptions,
    app_handle: AppHandle,
) -> anyhow::Result<SendResult> {
    let event_emitter = TransferEventEmitter::new(app_handle, Role::Sender);
    event_emitter.emit_started(TransferPhase::Preparing);
    let result = send_started(path, options, event_emitter.clone()).await;
    if let Err(error) = &result {
        if is_transfer_cancelled(error) {
            event_emitter.emit_cancelled();
        } else if let Some(details) = classified_transfer_error(error) {
            event_emitter.emit_failed(details);
        } else {
            event_emitter.emit_internal_failure("send failed");
        }
    }
    result
}

/// Run sender setup after the public entry point has opened its observable session.
async fn send_started(
    path: PathBuf,
    options: SendOptions,
    event_emitter: TransferEventEmitter,
) -> anyhow::Result<SendResult> {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    if ctrl_c_requested(ctrl_c.as_mut()).await {
        return Err(transfer_cancelled_error());
    }
    let plan = prepare_sender_plan(&path, &options, ctrl_c.as_mut()).await?;
    let endpoint = prepare_endpoint(&options).await.map_err(|error| {
        sender_failure(
            error,
            TransferErrorCode::ConnectionFailed,
            TransferPhase::Connecting,
            true,
            "unable to initialize sender networking",
        )
    })?;
    if ctrl_c_requested(ctrl_c.as_mut()).await {
        endpoint.close().await;
        return Err(transfer_cancelled_error());
    }
    let share_request = plan.build_request(path, event_emitter.clone());
    let setup_result = select! {
        biased;
        x = setup_data_sharing(
            endpoint,
            plan.blobs_data_dir.clone(),
            share_request
        ) => x,
        _ = &mut ctrl_c => {
            Err(transfer_cancelled_error())
        }
    };
    let setup = match setup_result {
        Ok(setup) => setup,
        Err(error) => {
            let cleanup_result = if is_transfer_cancelled(&error) {
                // 取消时 setup future 可能仍持有存储资源，先保留目录避免并发删除。
                Ok(())
            } else {
                remove_temp_sender_dir(&plan.blobs_data_dir).await
            };
            return Err(finalize_failed_sender_setup(error, cleanup_result));
        }
    };
    let online_wait_outcome = {
        let online_wait =
            wait_until_endpoint_is_online(setup.router.endpoint(), plan.wait_for_online);
        tokio::pin!(online_wait);
        select! {
            biased;
            _ = &mut ctrl_c => None,
            result = &mut online_wait => Some(result),
        }
    };
    match online_wait_outcome {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            return Err(shutdown_started_sender_setup(
                setup,
                plan.entry_type,
                plan.ticket_type,
                event_emitter.clone(),
                error,
            )
            .await);
        }
        None => {
            return Err(shutdown_started_sender_setup(
                setup,
                plan.entry_type,
                plan.ticket_type,
                event_emitter.clone(),
                transfer_cancelled_error(),
            )
            .await);
        }
    }
    let mut result = setup.into_send_result(plan.entry_type, plan.ticket_type, event_emitter);
    result.arm_session_lifetime(options.session_lifetime);
    info!(
        hash = %result.hash,
        size = result.size,
        entry_type = %result.entry_type,
        "send setup complete"
    );
    Ok(result)
}

async fn prepare_sender_plan<F>(
    path: &Path,
    options: &SendOptions,
    mut ctrl_c: Pin<&mut F>,
) -> anyhow::Result<SharePlan>
where
    F: Future<Output = std::io::Result<()>> + ?Sized,
{
    info!(
        relay_mode = ?options.relay_mode,
        ticket_type = ?options.ticket_type,
        manifest_mode = options.manifest_mode,
        max_upload_rate_bytes_per_sec = ?options.max_upload_rate_bytes_per_sec,
        max_receivers = ?options.max_receivers,
        max_files = ?options.max_files,
        max_total_size_bytes = ?options.max_total_size_bytes,
        max_import_memory_bytes = ?options.max_import_memory_bytes,
        session_lifetime = ?options.session_lifetime,
        "starting send"
    );
    options.validate().map_err(|error| {
        sender_failure(
            error,
            TransferErrorCode::InvalidInput,
            TransferPhase::Preparing,
            false,
            "invalid sender session settings",
        )
    })?;
    if ctrl_c_requested(ctrl_c.as_mut()).await {
        return Err(transfer_cancelled_error());
    }
    validate_share_path(path, options.manifest_mode).map_err(|error| {
        sender_failure(
            error,
            TransferErrorCode::InvalidInput,
            TransferPhase::Preparing,
            false,
            "invalid share path",
        )
    })?;
    if ctrl_c_requested(ctrl_c.as_mut()).await {
        return Err(transfer_cancelled_error());
    }
    if options.max_files.is_some() || options.max_total_size_bytes.is_some() {
        validate_share_limits(path, options.max_files, options.max_total_size_bytes).map_err(
            |error| {
                sender_failure(
                    error,
                    TransferErrorCode::InvalidInput,
                    TransferPhase::Preparing,
                    false,
                    "share exceeds the configured resource limit",
                )
            },
        )?;
    }
    if ctrl_c_requested(ctrl_c.as_mut()).await {
        return Err(transfer_cancelled_error());
    }
    let plan = SharePlan::new(path, options).map_err(|error| {
        sender_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Preparing,
            false,
            "unable to prepare sender storage",
        )
    })?;
    if ctrl_c_requested(ctrl_c.as_mut()).await {
        return Err(transfer_cancelled_error());
    }
    Ok(plan)
}

/// Start a share with no event consumer and return the opaque lifecycle handle.
pub async fn send_handle(
    path: PathBuf,
    options: SendOptions,
) -> anyhow::Result<SendHandle> {
    send_handle_with_events(path, options, None).await
}

/// Start a share and send lifecycle events to the supplied consumer.
pub async fn send_handle_with_events(
    path: PathBuf,
    options: SendOptions,
    app_handle: AppHandle,
) -> anyhow::Result<SendHandle> {
    send(path, options, app_handle)
        .await
        .map(SendResult::into_handle)
}

fn detect_entry_type(path: &Path) -> crate::core::types::EntryType {
    if path.is_file() {
        crate::core::types::EntryType::File
    } else {
        crate::core::types::EntryType::Directory
    }
}
