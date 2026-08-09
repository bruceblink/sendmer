//! 发送和接收结果定义。
//!
//! 本文件定义：SendResult, ReceiveResult。

use crate::core::types::EntryType;
use iroh_blobs::{Hash, ticket::BlobTicket};
use std::path::PathBuf;
use tokio::sync::watch;

pub use crate::core::progress::SenderTransferStatus;

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
    pub fn transfer_status(&self) -> SenderTransferStatus {
        *self.transfer_status_rx.borrow()
    }

    pub fn subscribe_transfer_status(&self) -> watch::Receiver<SenderTransferStatus> {
        self.transfer_status_rx.clone()
    }

    /// Shut down the active share, release store handles, and remove its temporary blob data.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        let Self {
            router,
            temp_tag,
            blobs_data_dir,
            _progress_handle,
            _store,
            ..
        } = self;

        drop(temp_tag);
        let shutdown_result = match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            router.shutdown(),
        )
        .await
        {
            Ok(result) => result.map_err(anyhow::Error::from),
            Err(error) => Err(error.into()),
        };

        // Windows cannot remove the blob directory while router/store handles still own files.
        drop(router);
        drop(_progress_handle);
        drop(_store);

        let cleanup_result =
            normalize_sender_cleanup_result(tokio::fs::remove_dir_all(&blobs_data_dir).await);
        finalize_sender_shutdown(shutdown_result, cleanup_result)
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
        options::{RelayModeOption, SendOptions},
        sender,
    };

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
    async fn sender_shutdown_removes_blob_store_after_releasing_handles() {
        let source_dir = tempfile::tempdir().expect("source directory");
        let source_file = source_dir.path().join("source.bin");
        tokio::fs::write(&source_file, b"shutdown cleanup")
            .await
            .expect("write source file");
        let options = SendOptions {
            relay_mode: RelayModeOption::Disabled,
            ..SendOptions::default()
        };

        let result = sender::send(source_file, options, None)
            .await
            .expect("start sender");
        let blobs_data_dir = result.blobs_data_dir.clone();
        assert!(blobs_data_dir.exists());

        result.shutdown().await.expect("shutdown sender");

        assert!(!blobs_data_dir.exists());
    }
}
