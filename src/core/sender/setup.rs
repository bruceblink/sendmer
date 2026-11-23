/// Set up data sharing and return immediately after the router starts.
///
/// The caller owns the returned setup before it waits for endpoint readiness, so a
/// later cancellation can use the normal graceful shutdown path.
async fn setup_data_sharing(
    endpoint: Endpoint,
    blobs_data_dir: PathBuf,
    share_request: ShareRequest,
) -> anyhow::Result<SharingSetup> {
    let cleanup_dir = blobs_data_dir.clone();
    let (progress_tx, progress_rx) = mpsc::channel(32);
    let (transfer_status_tx, transfer_status_rx) = watch::channel(SenderTransferStatus::Idle);
    let (shutdown_signal_tx, shutdown_signal_rx) = watch::channel(false);

    let setup_future = async move {
        let ShareRequest {
            path,
            entry_type,
            event_emitter,
            max_upload_rate_bytes_per_sec,
            max_receivers,
            max_files,
            max_total_size_bytes,
            max_import_memory_bytes,
            manifest_mode,
        } = share_request;
        let store = load_fs_store(&blobs_data_dir).await.map_err(|error| {
            sender_failure(
                error,
                TransferErrorCode::Filesystem,
                TransferPhase::Preparing,
                false,
                "unable to prepare sender storage",
            )
        })?;

        let blobs = BlobsProtocol::new(
            &store,
            Some(create_event_sender(
                progress_tx,
                max_upload_rate_bytes_per_sec,
                max_receivers,
            )),
        );

        let imported = import(path, blobs.store(), max_import_memory_bytes, manifest_mode)
            .await
            .map_err(|error| {
                sender_failure(
                    error,
                    TransferErrorCode::Filesystem,
                    TransferPhase::Preparing,
                    false,
                    "unable to import shared data",
                )
            })?;
        validate_imported_share_limits(&imported, max_files, max_total_size_bytes).map_err(
            |error| {
                sender_failure(
                    error,
                    TransferErrorCode::InvalidInput,
                    TransferPhase::Preparing,
                    false,
                    "imported share exceeds the configured resource limit",
                )
            },
        )?;
        let size = imported.size;
        let progress_handle = spawn_provider_progress_task(
            progress_rx,
            event_emitter,
            size,
            entry_type,
            transfer_status_tx.clone(),
            ProviderProgressControl {
                max_upload_rate_bytes_per_sec,
                max_receivers,
                shutdown_signal_rx,
            },
        );

        let router = iroh::protocol::Router::builder(endpoint)
            .accept(iroh_blobs::protocol::ALPN, blobs.clone())
            .spawn();

        anyhow::Ok(SharingSetup {
            router,
            imported,
            blobs_data_dir,
            store,
            progress_handle,
            transfer_status_tx,
            transfer_status_rx,
            shutdown_signal_tx,
        })
    };

    match setup_future.await {
        Ok(setup) => Ok(setup),
        Err(error) => Err(finalize_failed_sender_setup(
            error,
            remove_temp_sender_dir(&cleanup_dir).await,
        )),
    }
}

/// Remove a sender-owned temporary store after setup fails, tolerating a store
/// that was never created.
async fn remove_temp_sender_dir(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Preserve the startup error while recording a best-effort cleanup failure.
fn finalize_failed_sender_setup(
    primary_error: anyhow::Error,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Error {
    if cleanup_result.is_err() {
        warn!("failed to clean sender temporary data after setup error");
    }
    primary_error
}

/// Pair an internal sender error with the stable details exposed to event consumers.
fn sender_failure(
    error: anyhow::Error,
    code: TransferErrorCode,
    phase: TransferPhase,
    retryable: bool,
    message: &'static str,
) -> anyhow::Error {
    classify_transfer_error(error, TransferError::new(code, phase, retryable, message))
}

struct ShareRequest {
    path: PathBuf,
    entry_type: crate::core::types::EntryType,
    event_emitter: TransferEventEmitter,
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
    max_files: Option<NonZeroU64>,
    max_total_size_bytes: Option<NonZeroU64>,
    max_import_memory_bytes: Option<NonZeroU64>,
    manifest_mode: bool,
}

struct SharePlan {
    entry_type: crate::core::types::EntryType,
    wait_for_online: bool,
    blobs_data_dir: PathBuf,
    ticket_type: AddrInfoOptions,
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
    max_files: Option<NonZeroU64>,
    max_total_size_bytes: Option<NonZeroU64>,
    max_import_memory_bytes: Option<NonZeroU64>,
    manifest_mode: bool,
}

struct ImportedSource {
    name: String,
    path: PathBuf,
    size: u64,
    fingerprint: SourceFingerprint,
}

/// 文件导入前记录的元数据，用于确认打开的句柄仍对应扫描到的文件。
#[derive(Clone, Debug, PartialEq, Eq)]
struct SourceFingerprint {
    size: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl SourceFingerprint {
    /// 从文件元数据生成指纹；修改时间缺失时无法安全确认源文件身份。
    fn from_metadata(metadata: &std::fs::Metadata) -> anyhow::Result<Self> {
        Ok(Self {
            size: metadata.len(),
            modified: metadata
                .modified()
                .context("source file has no modification timestamp")?,
            #[cfg(unix)]
            device: {
                use std::os::unix::fs::MetadataExt;
                metadata.dev()
            },
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
        })
    }

    /// 比较打开句柄的元数据，拒绝扫描后被替换的路径。
    fn verify_open_metadata(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> anyhow::Result<()> {
        let actual = Self::from_metadata(metadata)?;
        anyhow::ensure!(
            self == &actual,
            "source file {} changed between scan and open",
            path.display()
        );
        Ok(())
    }
}

struct ImportedBlob {
    name: String,
    temp_tag: TempTag,
    size: u64,
}

struct ManifestSource {
    path: PathBuf,
    relative: ManifestPath,
    kind: ManifestEntryKind,
    size: u64,
    metadata: ManifestMetadata,
    fingerprint: Option<SourceFingerprint>,
}
