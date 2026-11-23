use super::shutdown_receive_store_and_cleanup_staging;
use super::{
    DownloadPlan, ReceiveRetryPolicy, await_receive_phase, base_endpoint_builder,
    cancellation_requested, close_receive_endpoint, collection_root_name, commit_staged_export,
    completed_local_total_files, completed_local_total_files_from_children, create_staging_dir,
    download_backoff, emit_receive_failed, export_atomically, export_manifest_atomically,
    finalize_cleanup, finalize_failed_receive, get_export_path,
    move_staged_root_without_replacing, process_export_stream,
    process_get_stream_with_reporter, receive, receive_failed_message, receive_failure,
    receive_with_limits_and_cancellation, resolve_output_dir, should_preserve_uncached_storage,
    size_fetch_backoff, validate_partial_receive_limits, validate_path_component,
    validate_receive_limits, wait_for_cancellation,
};
use crate::core::events::{
    EventEmitter, Role, TransferErrorCode, TransferEvent, TransferEventData, TransferPhase,
    classified_transfer_error,
};
use crate::core::manifest::{
    MANIFEST_ENTRY_PREFIX, ManifestEntry, ManifestEntryKind, ManifestMetadata, ManifestPath,
    ManifestPayload, ManifestRoot, TransferManifest,
};
use crate::core::options::{
    MAX_LEGACY_COLLECTION_ITEMS, ReceiveLimits, ReceiveOptions, RelayModeOption,
};
use crate::core::progress::{ReceiverProgressReporter, TransferEventEmitter};
use iroh_blobs::Hash;
use iroh_blobs::api::{blobs::ExportProgressItem, remote::GetProgressItem};
use iroh_blobs::format::collection::Collection;
use n0_future::stream;
use std::future::Future;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

fn poll_once_then_drop<F, T>(export: F)
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

fn find_staging_dir(output_dir: &Path) -> PathBuf {
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

async fn finish_cancelled_export_cleanup(
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

async fn assert_cached_payload(
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
struct RecordingEmitter {
    events: StdMutex<Vec<TransferEvent>>,
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

#[tokio::test]
async fn invalid_ticket_emits_structured_invalid_input_failure() {
    let emitter = Arc::new(RecordingEmitter::default());
    let error = receive(
        "not-a-ticket".to_owned(),
        ReceiveOptions::default(),
        Some(emitter.clone()),
    )
    .await
    .expect_err("invalid ticket should fail");

    assert!(!error.to_string().is_empty());
    let events = emitter.events();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].event, TransferEventData::Started));
    assert!(matches!(
        &events[1].event,
        TransferEventData::Failed { error }
            if error.code == TransferErrorCode::InvalidInput
                && error.phase == TransferPhase::Preparing
                && !error.retryable
    ));
}

fn receiver_reporter(
    app_handle: crate::core::events::AppHandle,
    total: u64,
) -> ReceiverProgressReporter {
    let emitter = TransferEventEmitter::new(app_handle, Role::Receiver);
    emitter.emit_started(TransferPhase::Transferring);
    ReceiverProgressReporter::new(emitter, total)
}

#[test]
fn validate_path_component_accepts_normal_name() {
    validate_path_component("report.txt").expect("regular filename should be allowed");
}

#[test]
fn validate_path_component_rejects_empty_name() {
    let err = validate_path_component("").expect_err("empty component should fail");
    assert!(err.to_string().contains("cannot be empty"));
}

#[test]
fn validate_path_component_rejects_path_traversal() {
    let err = validate_path_component("..").expect_err("parent traversal should fail");
    assert!(err.to_string().contains("path traversal"));
}

#[test]
fn validate_path_component_rejects_path_separator() {
    let err = validate_path_component("dir/file").expect_err("separator should fail");
    assert!(err.to_string().contains("must not contain path separators"));
}

#[test]
fn validate_path_component_rejects_windows_invalid_characters() {
    for component in [
        "bad<name",
        "bad:name",
        "bad\"name",
        "bad|name",
        "bad?name",
        "bad*name",
        "bad\u{0001}name",
    ] {
        let err = validate_path_component(component)
            .expect_err("Windows-invalid characters must be rejected");
        assert!(err.to_string().contains("not portable"));
    }
}

#[test]
fn validate_path_component_rejects_windows_trailing_dot_space_and_devices() {
    for component in [
        "report.", "report ", "CON", "con.txt", "COM1.log", "LPT9.csv",
    ] {
        let err = validate_path_component(component)
            .expect_err("Windows-invalid legacy names must be rejected");
        assert!(err.to_string().contains("not portable"));
    }
}

#[test]
fn get_export_path_joins_nested_relative_path() {
    let root = Path::new("downloads");
    let export_path = get_export_path(root, "dir/subdir/file.bin")
        .expect("nested relative path should be accepted");
    assert_eq!(
        export_path,
        root.join("dir").join("subdir").join("file.bin")
    );
}

#[test]
fn get_export_path_rejects_traversal_component() {
    let root = Path::new("downloads");
    let err = get_export_path(root, "../secret.txt").expect_err("traversal should fail");
    assert!(err.to_string().contains("path traversal"));
}

#[test]
fn get_export_path_rejects_empty_component() {
    let root = Path::new("downloads");
    let err = get_export_path(root, "dir//file.txt").expect_err("empty component should fail");
    assert!(err.to_string().contains("cannot be empty"));
}

#[test]
fn get_export_path_rejects_absolute_like_name() {
    let root = tempfile::tempdir()
        .expect("temp dir")
        .path()
        .join("downloads");
    let err = get_export_path(&root, "/etc/passwd")
        .expect_err("absolute-style export name should fail");
    assert!(err.to_string().contains("cannot be empty"));
}

#[test]
fn get_export_path_rejects_when_root_is_a_file() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root_file = temp_dir.path().join("not-a-dir");
    std::fs::write(&root_file, b"content").expect("write root file");

    let err =
        get_export_path(&root_file, "dir/file.txt").expect_err("file root should be rejected");
    assert!(err.to_string().contains("is not a directory"));
}

#[test]
fn get_export_path_rejects_existing_regular_file() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root = temp_dir.path().join("stage");
    let target = get_export_path(&root, "already-exported.txt")
        .expect("first staged target should be available");
    std::fs::write(&target, b"first entry").expect("write staged target");

    let error = get_export_path(&root, "already-exported.txt")
        .expect_err("an existing staging target must not be overwritten");

    assert!(error.to_string().contains("already exists"));
    assert_eq!(
        std::fs::read(target).expect("original file remains"),
        b"first entry"
    );
}

#[cfg(windows)]
#[test]
fn get_export_path_rejects_case_insensitive_staging_collision() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root = temp_dir.path().join("stage");
    let first_target =
        get_export_path(&root, "A.txt").expect("first staged target should be available");
    std::fs::write(&first_target, b"first entry").expect("write staged target");

    let error = get_export_path(&root, "a.txt")
        .expect_err("case-insensitive aliases must not overwrite staged data");

    assert!(error.to_string().contains("already exists"));
    assert_eq!(
        std::fs::read(first_target).expect("original file remains"),
        b"first entry"
    );
}

#[tokio::test]
async fn cancelled_legacy_export_cleans_staging_after_store_shutdown() {
    let output_temp = tempfile::tempdir().expect("output temp directory");
    let store_temp = tempfile::tempdir().expect("store temp directory");
    let output_dir = output_temp.path().join("downloads");
    let store_dir = store_temp.path().to_path_buf();
    let filesystem_store = iroh_blobs::store::fs::FsStore::load(&store_dir)
        .await
        .expect("load real filesystem store");
    let db: iroh_blobs::api::Store = filesystem_store.into();
    let payload = vec![0x5a; 16 * 1024 * 1024];
    let payload_size = payload.len() as u64;
    let payload_tag = db
        .add_bytes(payload)
        .temp_tag()
        .await
        .expect("store export payload");
    let payload_hash = payload_tag.hash();
    let collection: Collection = std::iter::once(("root.bin", payload_hash)).collect();
    let mut active_staging_dir = None;

    poll_once_then_drop(export_atomically(
        &db,
        collection,
        &output_dir,
        &mut active_staging_dir,
    ));
    let staging_dir = find_staging_dir(&output_dir);
    assert_cached_payload(&db, payload_hash, payload_size as usize, 0x5a).await;
    finish_cancelled_export_cleanup(&mut active_staging_dir, &staging_dir, &db, &output_dir)
        .await;
}
