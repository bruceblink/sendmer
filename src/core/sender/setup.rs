use super::import::{ImportedCollection, import};
use super::provider::{ProviderProgressControl, create_event_sender, spawn_provider_progress_task};
use super::validation::{
    detect_entry_type, prepare_temp_directory, validate_imported_share_limits,
};
use crate::core::events::{
    TransferError, TransferErrorCode, TransferPhase, classify_transfer_error,
};
use crate::core::options::{AddrInfoOptions, SendOptions, apply_options};
use crate::core::progress::{SenderTransferStatus, TransferEventEmitter};
use crate::core::results::SendResult;
use crate::core::storage::load_fs_store;
use iroh::Endpoint;
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, BlobsProtocol};
use n0_future::task::AbortOnDropHandle;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, watch};
use tracing::warn;

/// Set up data sharing and return immediately after the router starts.
///
/// The caller owns the returned setup before it waits for endpoint readiness, so a
/// later cancellation can use the normal graceful shutdown path.
pub(super) async fn setup_data_sharing(
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
pub(super) async fn remove_temp_sender_dir(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Preserve the startup error while recording a best-effort cleanup failure.
pub(super) fn finalize_failed_sender_setup(
    primary_error: anyhow::Error,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Error {
    if cleanup_result.is_err() {
        warn!("failed to clean sender temporary data after setup error");
    }
    primary_error
}

/// Pair an internal sender error with the stable details exposed to event consumers.
pub(super) fn sender_failure(
    error: anyhow::Error,
    code: TransferErrorCode,
    phase: TransferPhase,
    retryable: bool,
    message: &'static str,
) -> anyhow::Error {
    classify_transfer_error(error, TransferError::new(code, phase, retryable, message))
}

pub(super) struct ShareRequest {
    pub(super) path: PathBuf,
    pub(super) entry_type: crate::core::types::EntryType,
    pub(super) event_emitter: TransferEventEmitter,
    pub(super) max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    pub(super) max_receivers: Option<NonZeroU64>,
    pub(super) max_files: Option<NonZeroU64>,
    pub(super) max_total_size_bytes: Option<NonZeroU64>,
    pub(super) max_import_memory_bytes: Option<NonZeroU64>,
    pub(super) manifest_mode: bool,
}

pub(super) struct SharePlan {
    pub(super) entry_type: crate::core::types::EntryType,
    pub(super) wait_for_online: bool,
    pub(super) blobs_data_dir: PathBuf,
    pub(super) ticket_type: AddrInfoOptions,
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
    max_files: Option<NonZeroU64>,
    max_total_size_bytes: Option<NonZeroU64>,
    max_import_memory_bytes: Option<NonZeroU64>,
    manifest_mode: bool,
}

pub(super) struct SharingSetup {
    pub(super) router: iroh::protocol::Router,
    pub(super) imported: ImportedCollection,
    blobs_data_dir: PathBuf,
    store: FsStore,
    progress_handle: AbortOnDropHandle<anyhow::Result<()>>,
    transfer_status_tx: watch::Sender<SenderTransferStatus>,
    transfer_status_rx: watch::Receiver<SenderTransferStatus>,
    shutdown_signal_tx: watch::Sender<bool>,
}

impl SharePlan {
    pub(super) fn new(path: &Path, options: &SendOptions) -> anyhow::Result<Self> {
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

    pub(super) const fn build_request(
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
    pub(super) fn into_send_result(
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
pub(super) async fn shutdown_started_sender_setup(
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
