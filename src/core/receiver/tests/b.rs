use super::super::cleanup::should_preserve_uncached_storage;
use super::super::context::DownloadPlan;
use super::super::entry::{cancellation_requested, wait_for_cancellation};
use super::super::errors::receive_failed_message;
use super::super::limits::{
    completed_local_total_files, completed_local_total_files_from_children,
    validate_partial_receive_limits, validate_receive_limits,
};
use super::support::{
    assert_cached_payload, find_staging_dir, finish_cancelled_export_cleanup, poll_once_then_drop,
};
use crate::core::events::{TransferErrorCode, TransferPhase, classified_transfer_error};
use crate::core::filesystem::{
    collection_root_name, commit_staged_export, create_staging_dir, export_manifest_atomically,
    get_export_path, move_staged_root_without_replacing,
};
use crate::core::manifest::{
    MANIFEST_ENTRY_PREFIX, ManifestEntry, ManifestEntryKind, ManifestMetadata, ManifestPath,
    ManifestPayload, ManifestRoot, TransferManifest,
};
use crate::core::options::{MAX_LEGACY_COLLECTION_ITEMS, ReceiveLimits};
use iroh_blobs::Hash;
use iroh_blobs::format::collection::Collection;
use std::num::NonZeroU64;
use std::path::Path;

#[tokio::test]
async fn cancelled_manifest_export_cleans_staging_after_store_shutdown() {
    let output_temp = tempfile::tempdir().expect("output temp directory");
    let store_temp = tempfile::tempdir().expect("store temp directory");
    let output_dir = output_temp.path().join("downloads");
    let store_dir = store_temp.path().to_path_buf();
    let filesystem_store = iroh_blobs::store::fs::FsStore::load(&store_dir)
        .await
        .expect("load real filesystem store");
    let db: iroh_blobs::api::Store = filesystem_store.into();
    let payload = vec![0xa5; 16 * 1024 * 1024];
    let payload_size = payload.len() as u64;
    let payload_tag = db
        .add_bytes(payload)
        .temp_tag()
        .await
        .expect("store export payload");
    let payload_hash = payload_tag.hash();
    let collection_name = format!("{MANIFEST_ENTRY_PREFIX}00000000");
    let payload_path =
        ManifestPath::from_path(Path::new("root.bin"), true).expect("valid manifest path");
    let manifest = TransferManifest {
        schema_version: 1,
        root: ManifestRoot {
            kind: ManifestEntryKind::File,
            path: payload_path.clone(),
        },
        entries: vec![ManifestEntry {
            id: "00000000".to_owned(),
            kind: ManifestEntryKind::File,
            path: payload_path,
            payload: Some(ManifestPayload {
                collection_name: collection_name.clone(),
                size: payload_size,
            }),
            metadata: ManifestMetadata::default(),
        }],
    };
    manifest.validate().expect("valid TM1 fixture");
    let collection: Collection = std::iter::once((collection_name, payload_hash)).collect();
    let mut active_staging_dir = None;

    poll_once_then_drop(export_manifest_atomically(
        &db,
        &collection,
        &manifest,
        &output_dir,
        &mut active_staging_dir,
    ));
    let staging_dir = find_staging_dir(&output_dir);
    assert_cached_payload(&db, payload_hash, payload_size as usize, 0xa5).await;
    finish_cancelled_export_cleanup(&mut active_staging_dir, &staging_dir, &db, &output_dir).await;
}

#[test]
fn commit_staged_export_moves_complete_root_into_place() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let output_dir = temp_dir.path().join("downloads");
    std::fs::create_dir_all(&output_dir).expect("output directory");
    let staging_dir = create_staging_dir(&output_dir).expect("staging directory");
    let staged_root = staging_dir.join("root");
    std::fs::create_dir_all(&staged_root).expect("staged root");
    std::fs::write(staged_root.join("file.txt"), b"complete").expect("staged file");

    commit_staged_export(&staging_dir, &output_dir, "root").expect("staged export should commit");

    assert_eq!(
        std::fs::read(output_dir.join("root/file.txt")).expect("committed file"),
        b"complete"
    );
    assert!(!staging_dir.exists());
}

#[test]
fn commit_staged_export_succeeds_when_staging_cleanup_is_not_empty() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let output_dir = temp_dir.path().join("downloads");
    std::fs::create_dir_all(&output_dir).expect("output directory");
    let staging_dir = create_staging_dir(&output_dir).expect("staging directory");
    let staged_root = staging_dir.join("root");
    std::fs::create_dir_all(&staged_root).expect("staged root");
    std::fs::write(staged_root.join("file.txt"), b"complete").expect("staged file");
    std::fs::write(staging_dir.join("leftover.txt"), b"cleanup only").expect("leftover");

    commit_staged_export(&staging_dir, &output_dir, "root")
        .expect("the committed root must not be reported as a failure");

    assert_eq!(
        std::fs::read(output_dir.join("root/file.txt")).expect("committed file"),
        b"complete"
    );
    assert!(!staging_dir.exists(), "best-effort cleanup removes staging");
}

#[test]
fn commit_staged_export_classifies_existing_target() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let output_dir = temp_dir.path().join("downloads");
    std::fs::create_dir_all(output_dir.join("root")).expect("existing target");
    let staging_dir = create_staging_dir(&output_dir).expect("staging directory");
    std::fs::create_dir_all(staging_dir.join("root")).expect("staged root");

    let error = commit_staged_export(&staging_dir, &output_dir, "root")
        .expect_err("existing target should fail");
    let details = classified_transfer_error(&error).expect("structured target conflict");

    assert_eq!(details.code, TransferErrorCode::TargetConflict);
    assert_eq!(details.phase, TransferPhase::Exporting);
    assert!(!details.retryable);
}

#[test]
fn move_staged_root_does_not_replace_existing_target() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let source = temp_dir.path().join("staged-file");
    let target = temp_dir.path().join("existing-file");
    std::fs::write(&source, b"complete").expect("staged file");
    std::fs::write(&target, b"existing").expect("existing file");

    let error = move_staged_root_without_replacing(&source, &target)
        .expect_err("native move must refuse to replace an existing target");

    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(
        std::fs::read(&source).expect("staged file remains"),
        b"complete"
    );
    assert_eq!(
        std::fs::read(&target).expect("existing file remains"),
        b"existing"
    );
}

#[test]
fn collection_root_name_rejects_multiple_top_level_roots() {
    let collection = [
        ("first/file.txt", Hash::new(b"first")),
        ("second/file.txt", Hash::new(b"second")),
    ]
    .into_iter()
    .collect();

    let error = collection_root_name(&collection)
        .expect_err("multiple roots cannot be committed atomically");

    assert!(error.to_string().contains("multiple top-level roots"));
}

#[cfg(unix)]
#[test]
fn get_export_path_rejects_symlinked_parent_outside_root() {
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root = temp_dir.path().join("downloads");
    let outside = temp_dir.path().join("outside");
    std::fs::create_dir_all(&outside).expect("create outside directory");
    std::fs::create_dir_all(&root).expect("create output directory");
    symlink(&outside, root.join("nested")).expect("create parent symlink");

    let err = get_export_path(&root, "nested/file.txt")
        .expect_err("symlinked parent outside root should be rejected");
    assert!(err.to_string().contains("within the root"));
    assert!(
        !outside.join("file.txt").exists(),
        "outside target must not be created"
    );
}

#[cfg(unix)]
#[test]
fn get_export_path_rejects_dangling_target_symlink() {
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root = temp_dir.path().join("downloads");
    let outside_target = temp_dir.path().join("outside").join("file.txt");
    std::fs::create_dir_all(root.parent().expect("root parent")).expect("create temp root");
    std::fs::create_dir_all(&root).expect("create output directory");
    symlink(&outside_target, root.join("file.txt")).expect("create dangling target symlink");

    let err =
        get_export_path(&root, "file.txt").expect_err("dangling target symlink should be rejected");
    assert!(err.to_string().contains("symbolic link"));
    assert!(
        !outside_target.exists(),
        "outside target must remain absent"
    );
}

#[test]
fn completed_local_total_files_handles_empty_collection() {
    assert_eq!(completed_local_total_files(0), 0);
    assert_eq!(completed_local_total_files(1), 0);
    assert_eq!(completed_local_total_files(3), 2);
}

#[test]
fn download_plan_separates_collection_metadata_from_file_payload() {
    let hash_seq = [
        Hash::new(b"metadata"),
        Hash::new(b"first file"),
        Hash::new(b"second file"),
    ]
    .into_iter()
    .collect();
    let plan = DownloadPlan::from_hash_seq_and_sizes(&hash_seq, &[18, 100, 200])
        .expect("valid collection metadata");

    assert_eq!(plan.total_files, 2);
    assert_eq!(plan.payload_size, 300);
    assert_eq!(plan.transfer_total, 414);
}

#[test]
fn download_plan_rejects_metadata_size_overflow() {
    let hash_seq = [Hash::new(b"metadata"), Hash::new(b"payload")]
        .into_iter()
        .collect();
    let error = DownloadPlan::from_hash_seq_and_sizes(&hash_seq, &[u64::MAX, 1])
        .expect_err("metadata totals must reject u64 overflow");
    assert!(error.to_string().contains("size exceeds u64 range"));
}

#[test]
fn download_plan_rejects_mismatched_metadata_children() {
    let hash_seq = [Hash::new(b"metadata"), Hash::new(b"payload")]
        .into_iter()
        .collect();
    let error = DownloadPlan::from_hash_seq_and_sizes(&hash_seq, &[18])
        .expect_err("metadata child counts must match");
    assert!(error.to_string().contains("does not match"));
}

#[test]
fn download_plan_rejects_excessive_metadata_children() {
    let child_count = MAX_LEGACY_COLLECTION_ITEMS + 2;
    let hash_seq = std::iter::repeat_n(Hash::new(b"entry"), child_count).collect();
    let sizes = vec![0_u64; child_count];
    let error = DownloadPlan::from_hash_seq_and_sizes(&hash_seq, &sizes)
        .expect_err("metadata child count must be bounded");
    assert!(error.to_string().contains("too many children"));
}

#[test]
fn receive_limits_reject_remote_file_and_byte_totals() {
    let limits = ReceiveLimits {
        max_files: NonZeroU64::new(1),
        max_total_size_bytes: NonZeroU64::new(99),
    };
    assert!(validate_receive_limits(2, 10, limits).is_err());
    assert!(validate_receive_limits(1, 100, limits).is_err());
    validate_receive_limits(1, 99, limits).expect("limits should include boundary values");
}

#[test]
fn receive_limits_reject_partial_state_before_retry() {
    let hash_seq = [Hash::new(b"metadata"), Hash::new(b"payload")]
        .into_iter()
        .collect();
    let plan = DownloadPlan::from_hash_seq_and_sizes(&hash_seq, &[18, 100])
        .expect("valid collection metadata");
    let limits = ReceiveLimits {
        max_files: None,
        max_total_size_bytes: NonZeroU64::new(50),
    };
    let error = validate_partial_receive_limits(140, Some(2), &plan, limits)
        .expect_err("partial transfer beyond quota should be rejected");
    assert!(error.to_string().contains("byte limit"));
}

#[test]
fn completed_local_total_files_from_children_rejects_missing_children() {
    let err = completed_local_total_files_from_children(None)
        .expect_err("missing children should be rejected");
    assert!(err.to_string().contains("missing collection children"));
}

#[test]
fn receive_failed_message_wraps_error_with_prefix() {
    let message = receive_failed_message(&anyhow::anyhow!("boom"));
    assert_eq!(message, "error: boom");
}

#[test]
fn receive_failed_message_preserves_failure_stage_context() {
    let error = anyhow::anyhow!("connection refused").context("fetch remote collection sizes");
    let message = receive_failed_message(&error);
    assert_eq!(
        message,
        "error: fetch remote collection sizes: connection refused"
    );
}

#[test]
fn failed_store_shutdown_preserves_uncached_receive_storage() {
    assert!(should_preserve_uncached_storage(
        false,
        &Err(anyhow::anyhow!("store shutdown timed out")),
    ));
    assert!(!should_preserve_uncached_storage(false, &Ok(()),));
    assert!(!should_preserve_uncached_storage(
        true,
        &Err(anyhow::anyhow!("store shutdown timed out")),
    ));
}

#[tokio::test]
async fn cancellation_wait_returns_after_signal() {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    let wait = tokio::spawn(wait_for_cancellation(Some(receiver)));
    sender.send(true).expect("send cancellation signal");
    tokio::time::timeout(std::time::Duration::from_secs(1), wait)
        .await
        .expect("cancellation should wake the receive task")
        .expect("cancellation wait task should finish");
}

#[test]
fn cancellation_requested_reads_watch_state() {
    let (sender, receiver) = tokio::sync::watch::channel(false);
    assert!(!cancellation_requested(Some(&receiver)));
    sender.send(true).expect("send cancellation state");
    assert!(cancellation_requested(Some(&receiver)));
}
