fn create_event_sender(
    progress_tx: mpsc::Sender<iroh_blobs::provider::events::ProviderMessage>,
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
) -> EventSender {
    EventSender::new(
        progress_tx,
        provider_event_mask(max_upload_rate_bytes_per_sec, max_receivers),
    )
}

/// Build the provider subscription mask for optional throttling and admission control.
const fn provider_event_mask(
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
) -> EventMask {
    EventMask {
        connected: if max_receivers.is_some() {
            ConnectMode::Intercept
        } else {
            ConnectMode::Notify
        },
        get: RequestMode::NotifyLog,
        throttle: if max_upload_rate_bytes_per_sec.is_some() {
            ThrottleMode::Intercept
        } else {
            ThrottleMode::None
        },
        ..EventMask::DEFAULT
    }
}

fn spawn_provider_progress_task(
    progress_rx: mpsc::Receiver<iroh_blobs::provider::events::ProviderMessage>,
    event_emitter: TransferEventEmitter,
    total_file_size: u64,
    entry_type: crate::core::types::EntryType,
    transfer_status_tx: watch::Sender<SenderTransferStatus>,
    control: ProviderProgressControl,
) -> AbortOnDropHandle<anyhow::Result<()>> {
    AbortOnDropHandle::new(tokio::spawn(show_provide_progress_with_provider_tracker(
        progress_rx,
        event_emitter,
        total_file_size,
        entry_type,
        transfer_status_tx,
        control,
    )))
}

/// Bundle provider limits and the sender shutdown signal passed to the progress task.
struct ProviderProgressControl {
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
    shutdown_signal_rx: watch::Receiver<bool>,
}

/// 尝试取得一个进度跟踪任务配额，不在提供者事件循环中等待。
fn try_acquire_provider_progress_permit(
    request_task_limit: &std::sync::Arc<Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    request_task_limit.clone().try_acquire_owned().ok()
}

async fn wait_until_endpoint_is_online(
    endpoint: &iroh::Endpoint,
    wait_for_online: bool,
) -> anyhow::Result<()> {
    if wait_for_online {
        match tokio::time::timeout(ENDPOINT_ONLINE_WAIT_TIMEOUT, endpoint.online()).await {
            Ok(()) => {}
            Err(_error) => {
                warn!(
                    timeout_secs = ENDPOINT_ONLINE_WAIT_TIMEOUT.as_secs(),
                    "endpoint online probe timed out; continuing with available addresses"
                );
            }
        }
    }
    Ok(())
}

struct SharingSetup {
    router: iroh::protocol::Router,
    imported: ImportedCollection,
    blobs_data_dir: PathBuf,
    store: FsStore,
    progress_handle: AbortOnDropHandle<anyhow::Result<()>>,
    transfer_status_tx: watch::Sender<SenderTransferStatus>,
    transfer_status_rx: watch::Receiver<SenderTransferStatus>,
    shutdown_signal_tx: watch::Sender<bool>,
}

struct ImportedCollection {
    temp_tag: TempTag,
    file_count: u64,
    size: u64,
    _collection: Collection,
}

impl SharePlan {
    fn new(path: &Path, options: &SendOptions) -> anyhow::Result<Self> {
        Ok(Self {
            entry_type: detect_entry_type(path),
            wait_for_online: !matches!(
                options.relay_mode,
                crate::core::options::RelayModeOption::Disabled
            ),
            blobs_data_dir: prepare_temp_directory()?,
            ticket_type: options.ticket_type,
            max_upload_rate_bytes_per_sec: options.max_upload_rate_bytes_per_sec,
            max_receivers: options.max_receivers,
            max_files: options.max_files,
            max_total_size_bytes: options.max_total_size_bytes,
            max_import_memory_bytes: options.max_import_memory_bytes,
            manifest_mode: options.manifest_mode,
        })
    }

    const fn build_request(
        &self,
        path: PathBuf,
        event_emitter: TransferEventEmitter,
    ) -> ShareRequest {
        ShareRequest {
            path,
            entry_type: self.entry_type,
            event_emitter,
            max_upload_rate_bytes_per_sec: self.max_upload_rate_bytes_per_sec,
            max_receivers: self.max_receivers,
            max_files: self.max_files,
            max_total_size_bytes: self.max_total_size_bytes,
            max_import_memory_bytes: self.max_import_memory_bytes,
            manifest_mode: self.manifest_mode,
        }
    }
}

impl SharingSetup {
    fn into_send_result(
        self,
        entry_type: crate::core::types::EntryType,
        ticket_type: AddrInfoOptions,
        event_emitter: TransferEventEmitter,
    ) -> SendResult {
        let Self {
            router,
            imported,
            blobs_data_dir,
            store,
            progress_handle,
            transfer_status_tx,
            transfer_status_rx,
            shutdown_signal_tx,
        } = self;
        let ImportedCollection { temp_tag, size, .. } = imported;
        let hash = temp_tag.hash();

        let mut addr = router.endpoint().addr();
        apply_options(&mut addr, ticket_type);

        let ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq);

        SendResult {
            ticket,
            hash,
            size,
            entry_type,
            router,
            temp_tag,
            blobs_data_dir,
            _progress_handle: progress_handle,
            _store: store,
            transfer_status_tx,
            transfer_status_rx,
            event_emitter,
            shutdown_signal_tx,
            session_expiry_task: None,
        }
    }
}

/// Shut down a fully initialized share after its final startup step fails or is cancelled.
///
/// A `SharingSetup` already owns a live router and file-backed store, so it must be
/// converted into `SendResult` to reuse the ordered shutdown and directory cleanup logic.
async fn shutdown_started_sender_setup(
    setup: SharingSetup,
    entry_type: crate::core::types::EntryType,
    ticket_type: AddrInfoOptions,
    event_emitter: TransferEventEmitter,
    primary_error: anyhow::Error,
) -> anyhow::Error {
    let cleanup_result = setup
        .into_send_result(entry_type, ticket_type, event_emitter)
        .shutdown_resources()
        .await;
    finalize_failed_sender_setup(primary_error, cleanup_result)
}
