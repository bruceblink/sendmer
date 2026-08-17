//! 发送和接收结果定义。
//!
//! 本文件定义：SendResult, ReceiveResult。

use crate::core::events::{TransferError, TransferErrorCode, TransferPhase};
use crate::core::progress::TransferEventEmitter;
use crate::core::types::EntryType;
use iroh_blobs::{Hash, ticket::BlobTicket};
use std::path::PathBuf;
use tokio::sync::watch;

pub use crate::core::progress::SenderTransferStatus;

/// Opaque owner for an active send operation.
///
/// The handle exposes transfer metadata and lifecycle methods without leaking
/// the Router, blob store, or temporary-directory implementation details to
/// GUI and service callers.
pub struct SendHandle {
    inner: SendResult,
}

/// 发送结果结构体。
pub struct SendResult {
    pub ticket: BlobTicket,
    pub hash: Hash,
    pub size: u64,
    pub entry_type: EntryType,

    // CRITICAL: These fields must be kept alive for the duration of the share
    pub router: iroh::protocol::Router, // Keeps the server running and protocols active
    pub temp_tag: iroh_blobs::api::TempTag, // Prevents data from being garbage collected
    pub blobs_data_dir: PathBuf,        // Path for cleanup when share stops
    pub _progress_handle: n0_future::task::AbortOnDropHandle<anyhow::Result<()>>, // Keeps event channel open
    pub _store: iroh_blobs::store::fs::FsStore, // Keeps the blob storage alive
    pub(crate) transfer_status_rx: watch::Receiver<SenderTransferStatus>,
    pub(crate) event_emitter: TransferEventEmitter,
}

fn normalize_sender_cleanup_result(cleanup_result: std::io::Result<()>) -> anyhow::Result<()> {
    match cleanup_result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn finalize_sender_shutdown(
    shutdown_result: anyhow::Result<()>,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    if let Err(error) = cleanup_result {
        tracing::warn!(error = %error, "failed to clean sender temporary data dir");
    }
    shutdown_result
}

impl SendResult {
    /// Convert the legacy result into the stable lifecycle handle API.
    pub const fn into_handle(self) -> SendHandle {
        SendHandle { inner: self }
    }

    pub fn transfer_status(&self) -> SenderTransferStatus {
        *self.transfer_status_rx.borrow()
    }

    pub fn subscribe_transfer_status(&self) -> watch::Receiver<SenderTransferStatus> {
        self.transfer_status_rx.clone()
    }

    /// Shut down the active share, release store handles, and remove its temporary blob data.
    ///
    /// Iroh may need a few seconds to notify peers before closing an unhealthy QUIC
    /// connection. Wait for that shutdown so file-backed handles are released before
    /// removing the sender-owned directory.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let event_emitter = self.event_emitter.clone();
        let result = self.shutdown_resources().await;
        match &result {
            Ok(()) => event_emitter.emit_completed(),
            Err(_) => event_emitter.emit_failed(sender_finalization_error()),
        }
        result
    }

    /// Cancel the active share and emit the mutually exclusive cancelled terminal event.
    pub async fn cancel(self) -> anyhow::Result<()> {
        let event_emitter = self.event_emitter.clone();
        let result = self.shutdown_resources().await;
        match &result {
            Ok(()) => event_emitter.emit_cancelled(),
            Err(_) => event_emitter.emit_failed(sender_finalization_error()),
        }
        result
    }

    /// Release sender resources without deciding the public session terminal state.
    pub(crate) async fn shutdown_resources(self) -> anyhow::Result<()> {
        let Self {
            router,
            temp_tag,
            blobs_data_dir,
            _progress_handle,
            _store,
            ..
        } = self;

        drop(temp_tag);
        let shutdown_result = router.shutdown().await.map_err(anyhow::Error::from);

        // Windows cannot remove the blob directory while router/store handles still own files.
        drop(router);
        drop(_progress_handle);
        drop(_store);

        let cleanup_result =
            normalize_sender_cleanup_result(tokio::fs::remove_dir_all(&blobs_data_dir).await);
        finalize_sender_shutdown(shutdown_result, cleanup_result)
    }
}

fn sender_finalization_error() -> TransferError {
    TransferError::new(
        TransferErrorCode::Internal,
        TransferPhase::Finalizing,
        false,
        "unable to finalize sender resources",
    )
}

impl SendHandle {
    /// Return the ticket that peers can use to receive this share.
    pub const fn ticket(&self) -> &BlobTicket {
        &self.inner.ticket
    }

    /// Return the collection hash advertised by the ticket.
    pub const fn hash(&self) -> Hash {
        self.inner.hash
    }

    /// Return the imported payload size in bytes.
    pub const fn size(&self) -> u64 {
        self.inner.size
    }

    /// Return whether the share represents a single file or a directory.
    pub const fn entry_type(&self) -> EntryType {
        self.inner.entry_type
    }

    /// Read the current aggregate sender status.
    pub fn transfer_status(&self) -> SenderTransferStatus {
        self.inner.transfer_status()
    }

    /// Subscribe to aggregate sender status changes without exposing internals.
    pub fn subscribe_transfer_status(&self) -> watch::Receiver<SenderTransferStatus> {
        self.inner.subscribe_transfer_status()
    }

    /// Gracefully close the share and remove its temporary data.
    pub async fn close(self) -> anyhow::Result<()> {
        self.inner.shutdown().await
    }

    /// Cancel the share using the same ordered cleanup as `close`.
    pub async fn cancel(self) -> anyhow::Result<()> {
        self.inner.cancel().await
    }
}

/// 接收结果结构体。
#[derive(Debug)]
pub struct ReceiveResult {
    pub message: String,
    pub file_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::{finalize_sender_shutdown, normalize_sender_cleanup_result};
    use crate::core::{
        events::{EventEmitter, TransferEvent, TransferEventData},
        options::{RelayModeOption, SendOptions},
        sender,
    };
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingEmitter {
        events: Mutex<Vec<TransferEvent>>,
    }

    impl RecordingEmitter {
        fn events(&self) -> Vec<TransferEvent> {
            self.events.lock().expect("events lock").clone()
        }
    }

    impl EventEmitter for RecordingEmitter {
        fn emit(&self, event: &TransferEvent) {
            self.events.lock().expect("events lock").push(event.clone());
        }
    }

    #[test]
    fn normalize_sender_cleanup_result_ignores_not_found() {
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing dir");
        normalize_sender_cleanup_result(Err(err)).expect("not found should be ignored");
    }

    #[test]
    fn finalize_sender_shutdown_preserves_shutdown_error() {
        let shutdown_error = anyhow::anyhow!("shutdown failed");
        let cleanup_error = anyhow::anyhow!("cleanup failed");
        let err = finalize_sender_shutdown(Err(shutdown_error), Err(cleanup_error))
            .expect_err("shutdown error should be preserved");
        assert!(err.to_string().contains("shutdown failed"));
    }

    #[test]
    fn finalize_sender_shutdown_returns_ok_when_shutdown_succeeds() {
        finalize_sender_shutdown(Ok(()), Err(anyhow::anyhow!("cleanup failed")))
            .expect("cleanup errors should not fail successful shutdown");
    }

    #[tokio::test]
    async fn sender_shutdown_closes_endpoint_before_removing_blob_store() {
        let source_dir = tempfile::tempdir().expect("source directory");
        let source_file = source_dir.path().join("source.bin");
        tokio::fs::write(&source_file, b"shutdown cleanup")
            .await
            .expect("write source file");
        let options = SendOptions {
            relay_mode: RelayModeOption::Disabled,
            ..SendOptions::default()
        };
        let emitter = Arc::new(RecordingEmitter::default());

        let result = sender::send(source_file, options, Some(emitter.clone()))
            .await
            .expect("start sender");
        let blobs_data_dir = result.blobs_data_dir.clone();
        let endpoint = result.router.endpoint().clone();
        assert!(blobs_data_dir.exists());

        result.shutdown().await.expect("shutdown sender");

        assert!(endpoint.is_closed(), "shutdown should close the endpoint");
        assert!(!blobs_data_dir.exists());
        let events = emitter.events();
        assert!(matches!(events[0].event, TransferEventData::Started));
        assert!(matches!(events[1].event, TransferEventData::Completed));
        assert_eq!(events[0].session_id, events[1].session_id);
        assert_eq!([events[0].sequence, events[1].sequence], [1, 2]);
    }

    #[tokio::test]
    async fn send_handle_cancel_emits_cancelled_and_orders_cleanup() {
        let source_dir = tempfile::tempdir().expect("source directory");
        let source_file = source_dir.path().join("handle.bin");
        tokio::fs::write(&source_file, b"opaque handle")
            .await
            .expect("write source file");
        let options = SendOptions {
            relay_mode: RelayModeOption::Disabled,
            ..SendOptions::default()
        };
        let emitter = Arc::new(RecordingEmitter::default());

        let result = sender::send(source_file, options, Some(emitter.clone()))
            .await
            .expect("start sender");
        let expected_ticket = result.ticket.clone();
        let expected_hash = result.hash;
        let expected_size = result.size;
        let expected_entry_type = result.entry_type;
        let blobs_data_dir = result.blobs_data_dir.clone();
        let endpoint = result.router.endpoint().clone();

        let handle = result.into_handle();
        assert_eq!(handle.ticket(), &expected_ticket);
        assert_eq!(handle.hash(), expected_hash);
        assert_eq!(handle.size(), expected_size);
        assert_eq!(handle.entry_type(), expected_entry_type);
        handle.cancel().await.expect("cancel sender handle");

        assert!(endpoint.is_closed(), "cancel should close the endpoint");
        assert!(!blobs_data_dir.exists());
        let events = emitter.events();
        assert!(matches!(events[0].event, TransferEventData::Started));
        assert!(matches!(events[1].event, TransferEventData::Cancelled));
        assert_eq!(events[0].session_id, events[1].session_id);
        assert_eq!([events[0].sequence, events[1].sequence], [1, 2]);
    }
}
