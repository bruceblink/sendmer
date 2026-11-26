use super::super::cleanup::shutdown_receive_store_and_cleanup_staging;
use crate::core::events::{EventEmitter, Role, TransferEvent, TransferPhase};
use crate::core::progress::{ReceiverProgressReporter, TransferEventEmitter};
use iroh_blobs::Hash;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::task::{Context, Poll, Waker};

pub(super) fn poll_once_then_drop<F, T>(export: F)
where
    F: Future<Output = anyhow::Result<T>>,
{
    let mut export = Box::pin(export);
    let mut task_context = Context::from_waker(Waker::noop());

    assert!(
        matches!(export.as_mut().poll(&mut task_context), Poll::Pending),
        "the export should suspend while the real blob store handles its request"
    );
    drop(export);
}

pub(super) fn find_staging_dir(output_dir: &Path) -> PathBuf {
    let mut staging_dirs = std::fs::read_dir(output_dir)
        .expect("output directory exists before export")
        .map(|entry| entry.expect("output directory entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".sendmer-stage-"))
        });
    let staging_dir = staging_dirs.next().expect("export created staging");
    assert!(
        staging_dirs.next().is_none(),
        "one active staging directory"
    );
    staging_dir
}

pub(super) async fn finish_cancelled_export_cleanup(
    active_staging_dir: &mut Option<PathBuf>,
    staging_dir: &Path,
    db: &iroh_blobs::api::Store,
    output_dir: &Path,
) {
    assert!(staging_dir.is_dir(), "staging exists before cancellation");
    shutdown_receive_store_and_cleanup_staging(db, active_staging_dir)
        .await
        .expect("drain and shut down store before staging cleanup");

    assert!(
        !output_dir.join("root.bin").exists(),
        "cancelled export never commits its root"
    );
    assert!(!staging_dir.exists(), "cancelled export removes staging");
    assert!(active_staging_dir.is_none(), "staging ownership is cleared");
    assert!(output_dir.is_dir(), "the requested output root remains");
}

pub(super) async fn assert_cached_payload(
    db: &iroh_blobs::api::Store,
    payload_hash: Hash,
    payload_len: usize,
    payload_byte: u8,
) {
    let cached = db
        .get_bytes(payload_hash)
        .await
        .expect("cancelled export keeps cached data");
    assert_eq!(cached.len(), payload_len);
    assert!(cached.iter().all(|byte| *byte == payload_byte));
}

#[derive(Default)]
pub(super) struct RecordingEmitter {
    events: StdMutex<Vec<TransferEvent>>,
}

impl RecordingEmitter {
    pub(super) fn events(&self) -> Vec<TransferEvent> {
        self.events.lock().expect("events lock").clone()
    }
}

impl EventEmitter for RecordingEmitter {
    fn emit(&self, event: &TransferEvent) {
        self.events.lock().expect("events lock").push(event.clone());
    }
}

pub(super) fn receiver_reporter(
    app_handle: crate::core::events::AppHandle,
    total: u64,
) -> ReceiverProgressReporter {
    let emitter = TransferEventEmitter::new(app_handle, Role::Receiver);
    emitter.emit_started(TransferPhase::Transferring);
    ReceiverProgressReporter::new(emitter, total)
}
