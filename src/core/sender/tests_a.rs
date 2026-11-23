use super::{
    ImportMemoryBudget, MANIFEST_COLLECTION_NAME, ProviderProgressControl, ReceiverAdmission,
    ShareRequest, UploadRateLimiter, canonicalized_path_to_string, collect_import_sources,
    ctrl_c_requested, detect_entry_type, import_parallelism, prepare_endpoint,
    provider_event_mask, send, setup_data_sharing, show_provide_progress_with_provider_tracker,
    shutdown_started_sender_setup, try_acquire_provider_progress_permit,
    validate_imported_share_totals, validate_legacy_collection_name, validate_share_limits,
    validate_share_path,
};
use crate::core::events::{
    EventEmitter, Role, TransferErrorCode, TransferEvent, TransferEventData, TransferPhase,
};
use crate::core::options::{AddrInfoOptions, RelayModeOption, SendOptions, apply_options};
use crate::core::progress::{SenderTransferStatus, TransferEventEmitter};
use crate::core::types::EntryType;
use iroh::{EndpointAddr, RelayUrl, SecretKey, TransportAddr};
use iroh_blobs::provider::events::{
    ClientConnected, ConnectMode, ConnectionClosed, EventSender, ProgressError, ThrottleMode,
};
use std::num::NonZeroU64;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Semaphore, watch};

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

#[tokio::test]
async fn ctrl_c_requested_checks_without_waiting() {
    let ready = async { Ok(()) };
    tokio::pin!(ready);
    assert!(ctrl_c_requested(ready.as_mut()).await);

    let pending = std::future::pending::<std::io::Result<()>>();
    tokio::pin!(pending);
    assert!(!ctrl_c_requested(pending.as_mut()).await);
}

#[test]
fn provider_event_mask_only_enables_throttle_for_configured_limits() {
    assert_eq!(provider_event_mask(None, None).throttle, ThrottleMode::None);
    assert_eq!(
        provider_event_mask(NonZeroU64::new(1_024), None).throttle,
        ThrottleMode::Intercept
    );
}

#[test]
fn provider_event_mask_intercepts_connections_for_receiver_limits() {
    assert_eq!(
        provider_event_mask(None, NonZeroU64::new(1)).connected,
        ConnectMode::Intercept
    );
    assert_eq!(
        provider_event_mask(None, None).connected,
        ConnectMode::Notify
    );
}

#[test]
fn receiver_admission_releases_slots_after_disconnect() {
    let mut admission = ReceiverAdmission::new(NonZeroU64::new(1));
    assert!(admission.admit(10));
    assert!(!admission.admit(20));
    admission.release(10);
    assert!(admission.admit(20));
}

#[test]
fn receiver_admission_allows_repeated_events_for_same_connection() {
    let mut admission = ReceiverAdmission::new(NonZeroU64::new(1));
    assert!(admission.admit(10));
    assert!(admission.admit(10));
}

#[test]
fn upload_delay_rounds_up_to_preserve_the_rate_ceiling() {
    assert_eq!(
        super::upload_delay(1, NonZeroU64::new(3).expect("non-zero rate")),
        Duration::from_nanos(333_333_334)
    );
    assert_eq!(
        super::upload_delay(
            16 * 1024,
            NonZeroU64::new(16 * 1024).expect("non-zero rate")
        ),
        Duration::from_secs(1)
    );
}

#[tokio::test(start_paused = true)]
async fn upload_rate_limiter_reserves_one_schedule_for_all_receivers() {
    let limiter = UploadRateLimiter::new(NonZeroU64::new(100).expect("non-zero rate"));
    let first = limiter.reserve(100).await;
    let second = limiter.reserve(100).await;

    assert_eq!(second.duration_since(first), Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn upload_rate_limiter_serializes_parallel_receiver_reservations() {
    let limiter = std::sync::Arc::new(UploadRateLimiter::new(
        NonZeroU64::new(100).expect("non-zero rate"),
    ));
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let first_limiter = limiter.clone();
    let first_barrier = barrier.clone();
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_limiter.reserve(100).await
    });
    let second_limiter = limiter;
    let second_barrier = barrier.clone();
    let second = tokio::spawn(async move {
        second_barrier.wait().await;
        second_limiter.reserve(100).await
    });

    barrier.wait().await;
    let first = first.await.expect("first reservation task");
    let second = second.await.expect("second reservation task");
    let (earlier, later) = if first <= second {
        (first, second)
    } else {
        (second, first)
    };

    assert_eq!(later.duration_since(earlier), Duration::from_secs(1));
}

#[tokio::test(start_paused = true)]
async fn upload_rate_limiter_cancels_pending_wait() {
    let limiter = std::sync::Arc::new(UploadRateLimiter::new(
        NonZeroU64::new(100).expect("non-zero rate"),
    ));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (_initial_shutdown_tx, mut initial_shutdown_rx) = watch::channel(false);
    assert!(
        limiter
            .wait_for_chunk_or_shutdown(100, &mut initial_shutdown_rx)
            .await
    );

    let wait_limiter = limiter.clone();
    let mut wait_shutdown = shutdown_rx;
    let pending = tokio::spawn(async move {
        wait_limiter
            .wait_for_chunk_or_shutdown(100, &mut wait_shutdown)
            .await
    });
    tokio::task::yield_now().await;

    shutdown_tx.send(true).expect("shutdown signal receiver");
    assert!(!pending.await.expect("pending wait task"));
}

#[tokio::test]
async fn import_memory_budget_serializes_estimated_source_bytes() {
    let budget = std::sync::Arc::new(ImportMemoryBudget::new(
        NonZeroU64::new(100).expect("non-zero budget"),
    ));
    let first = budget.acquire(80).await;
    let (started_tx, mut started_rx) = tokio::sync::watch::channel(false);
    let second_budget = budget.clone();
    let second = tokio::spawn(async move {
        let _permit = second_budget.acquire(40).await;
        started_tx.send(true).expect("started receiver");
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(50), started_rx.changed())
            .await
            .is_err(),
        "the second import should wait for available budget"
    );
    drop(first);
    tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .expect("second import should resume after release")
        .expect("second import task should join");
    assert!(*started_rx.borrow());
}

#[tokio::test]
async fn import_memory_budget_allows_one_source_larger_than_limit() {
    let budget = std::sync::Arc::new(ImportMemoryBudget::new(
        NonZeroU64::new(100).expect("non-zero budget"),
    ));
    let _permit = budget.acquire(1_000).await;
}

#[tokio::test]
async fn provider_progress_task_stops_when_sender_shutdown_is_signaled() {
    let (_event_sender, progress_rx) = EventSender::channel(8, provider_event_mask(None, None));
    let (transfer_status_tx, _transfer_status_rx) = watch::channel(SenderTransferStatus::Idle);
    let (shutdown_signal_tx, shutdown_signal_rx) = watch::channel(false);
    let progress_task = tokio::spawn(show_provide_progress_with_provider_tracker(
        progress_rx,
        TransferEventEmitter::new(None, Role::Sender),
        0,
        EntryType::File,
        transfer_status_tx,
        ProviderProgressControl {
            max_upload_rate_bytes_per_sec: None,
            max_receivers: None,
            shutdown_signal_rx,
        },
    ));

    shutdown_signal_tx
        .send(true)
        .expect("shutdown signal receiver");
    tokio::time::timeout(Duration::from_secs(1), progress_task)
        .await
        .expect("progress task should stop promptly")
        .expect("progress task should join")
        .expect("progress task should shut down cleanly");
}

#[tokio::test]
async fn provider_progress_permit_does_not_wait_when_limit_is_full() {
    let request_task_limit = std::sync::Arc::new(Semaphore::new(1));
    let held = request_task_limit
        .clone()
        .acquire_owned()
        .await
        .expect("initial permit");

    assert!(try_acquire_provider_progress_permit(&request_task_limit).is_none());
    drop(held);
    assert!(try_acquire_provider_progress_permit(&request_task_limit).is_some());
}

#[tokio::test]
async fn provider_rejects_connections_after_receiver_limit_and_reopens_slots() {
    let max_receivers = NonZeroU64::new(1);
    let (event_sender, progress_rx) =
        EventSender::channel(8, provider_event_mask(None, max_receivers));
    let (transfer_status_tx, _transfer_status_rx) = watch::channel(SenderTransferStatus::Idle);
    let (_shutdown_signal_tx, shutdown_signal_rx) = watch::channel(false);
    let progress_task = tokio::spawn(show_provide_progress_with_provider_tracker(
        progress_rx,
        TransferEventEmitter::new(None, Role::Sender),
        0,
        EntryType::File,
        transfer_status_tx,
        ProviderProgressControl {
            max_upload_rate_bytes_per_sec: None,
            max_receivers,
            shutdown_signal_rx,
        },
    ));

    event_sender
        .client_connected(|| ClientConnected {
            connection_id: 1,
            endpoint_id: None,
        })
        .await
        .expect("first connection should be admitted");
    let error = event_sender
        .client_connected(|| ClientConnected {
            connection_id: 2,
            endpoint_id: None,
        })
        .await
        .expect_err("second connection should be rejected");
    assert!(matches!(error, ProgressError::Limit { .. }));

    event_sender
        .connection_closed(|| ConnectionClosed { connection_id: 1 })
        .await
        .expect("closing a connection should release its slot");
    event_sender
        .client_connected(|| ClientConnected {
            connection_id: 2,
            endpoint_id: None,
        })
        .await
        .expect("a later connection should reuse the released slot");

    drop(event_sender);
    progress_task
        .await
        .expect("progress task should join")
        .expect("progress task should shut down cleanly");
}

fn sample_addr() -> iroh::EndpointAddr {
    let node_id = SecretKey::generate().public();
    let relay = RelayUrl::from_str("https://relay.example").expect("valid relay url");
    let ip = "127.0.0.1:7777".parse().expect("valid socket addr");
    EndpointAddr::new(node_id)
        .with_relay_url(relay)
        .with_ip_addr(ip)
}

#[test]
fn apply_options_matches_ticket_type_semantics() {
    let base = sample_addr();

    let mut id_only = base.clone();
    apply_options(&mut id_only, AddrInfoOptions::Id);
    assert!(id_only.addrs.is_empty());

    let mut relay_only = base.clone();
    apply_options(&mut relay_only, AddrInfoOptions::Relay);
    assert!(
        relay_only
            .addrs
            .iter()
            .all(|addr| matches!(addr, TransportAddr::Relay(_)))
    );
    assert!(!relay_only.addrs.is_empty());

    let mut ip_only = base.clone();
    apply_options(&mut ip_only, AddrInfoOptions::Addresses);
    assert!(
        ip_only
            .addrs
            .iter()
            .all(|addr| matches!(addr, TransportAddr::Ip(_)))
    );
    assert!(!ip_only.addrs.is_empty());

    let mut full = base.clone();
    apply_options(&mut full, AddrInfoOptions::RelayAndAddresses);
    assert_eq!(full.addrs.len(), base.addrs.len());
}

#[test]
fn disabled_relay_skips_online_wait() {
    let wait_for_online = !matches!(
        crate::core::options::RelayModeOption::Disabled,
        crate::core::options::RelayModeOption::Disabled
    );
    assert!(!wait_for_online);
}

#[test]
fn canonicalized_relative_path_uses_forward_slashes() {
    let path = Path::new("folder").join("nested").join("file.txt");
    let value = canonicalized_path_to_string(&path, true).expect("path should convert");
    assert_eq!(value, "folder/nested/file.txt");
}

#[test]
fn canonicalized_absolute_path_keeps_leading_slash_when_allowed() {
    let value = canonicalized_path_to_string(Path::new("/folder/file.txt"), false)
        .expect("absolute path should convert");
    assert_eq!(value, "/folder/file.txt");
}

#[test]
fn canonicalized_absolute_path_is_rejected_when_relative_required() {
    let err = canonicalized_path_to_string(Path::new("/folder/file.txt"), true)
        .expect_err("absolute path should be rejected");
    assert!(err.to_string().contains("invalid path component"));
}

#[test]
fn detect_entry_type_distinguishes_file_and_directory() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let file_path = temp_dir.path().join("demo.txt");
    std::fs::write(&file_path, b"demo").expect("write file");

    assert_eq!(detect_entry_type(&file_path), EntryType::File);
    assert_eq!(detect_entry_type(temp_dir.path()), EntryType::Directory);
}

#[test]
fn collect_import_sources_returns_relative_sorted_names_after_sorting() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root = temp_dir.path().join("data");
    let nested = root.join("nested");
    std::fs::create_dir_all(&nested).expect("create dirs");
    std::fs::write(root.join("alpha.txt"), b"a").expect("write alpha");
    std::fs::write(nested.join("beta.txt"), b"b").expect("write beta");

    let mut names = collect_import_sources(root)
        .expect("sources")
        .into_iter()
        .map(|source| source.name)
        .collect::<Vec<_>>();
    names.sort();

    assert_eq!(names, vec!["data/alpha.txt", "data/nested/beta.txt"]);
}

#[test]
fn source_fingerprint_rejects_replaced_file() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let path = temp_dir.path().join("source.txt");
    std::fs::write(&path, b"original").expect("write original source");

    let source = collect_import_sources(path.clone())
        .expect("collect source")
        .into_iter()
        .next()
        .expect("source entry");
    std::fs::write(&path, b"replacement with a different size").expect("replace source");

    let metadata = std::fs::metadata(&path).expect("read replacement metadata");
    let error = source
        .fingerprint
        .verify_open_metadata(&path, &metadata)
        .expect_err("replacement must fail fingerprint validation");
    assert!(error.to_string().contains("changed between scan and open"));
}

#[test]
fn legacy_collection_rejects_manifest_control_name() {
    let error = validate_legacy_collection_name(MANIFEST_COLLECTION_NAME)
        .expect_err("manifest control name must remain reserved");
    assert!(error.to_string().contains("reserved"));
}

#[test]
fn import_parallelism_is_bounded_by_available_work() {
    assert_eq!(import_parallelism(0), 1);
    assert_eq!(import_parallelism(1), 1);
    assert!(import_parallelism(usize::MAX) >= 1);
}

#[test]
fn validate_share_path_rejects_current_directory_aliases() {
    let dot_err = validate_share_path(Path::new("."), false)
        .expect_err("`.` should be treated as current directory");
    assert!(dot_err.to_string().contains("current directory"));

    let dot_slash_err = validate_share_path(Path::new("./"), false)
        .expect_err("`./` should be treated as current directory");
    assert!(dot_slash_err.to_string().contains("current directory"));
}

#[test]
fn validate_share_path_rejects_current_directory_absolute_path() {
    let cwd = std::env::current_dir().expect("current dir");
    let err = validate_share_path(&cwd, false)
        .expect_err("absolute current directory should be rejected");
    assert!(err.to_string().contains("current directory"));
}

#[test]
fn validate_share_path_rejects_missing_path() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let missing = temp_dir.path().join("missing-share");

    let err =
        validate_share_path(&missing, false).expect_err("missing path should be rejected");

    assert!(
        err.to_string()
            .contains("does not exist or cannot be accessed")
    );
}

#[test]
fn validate_share_path_accepts_directory_with_regular_files() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let nested = temp_dir.path().join("nested").join("share");
    std::fs::create_dir_all(&nested).expect("create nested dir");
    std::fs::write(nested.join("file.txt"), b"content").expect("write source file");
    validate_share_path(&nested, false).expect("nested path should be accepted");
}

#[test]
fn validate_share_limits_rejects_directory_over_file_limit() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root = temp_dir.path().join("share");
    std::fs::create_dir_all(&root).expect("create share directory");
    std::fs::write(root.join("one.txt"), b"one").expect("write first file");
    std::fs::write(root.join("two.txt"), b"two").expect("write second file");

    let error = validate_share_limits(&root, NonZeroU64::new(1), None)
        .expect_err("file limit should reject the second file");

    assert!(error.to_string().contains("more than 1 files"));
    assert!(error.to_string().contains("max-files"));
}

#[test]
fn validate_share_limits_accepts_file_at_limits() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let file = temp_dir.path().join("share.txt");
    std::fs::write(&file, b"content").expect("write share file");

    validate_share_limits(&file, NonZeroU64::new(1), NonZeroU64::new(7))
        .expect("file at both limits should be accepted");
}

#[test]
fn validate_share_limits_rejects_directory_over_total_size_limit() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root = temp_dir.path().join("share");
    std::fs::create_dir_all(&root).expect("create share directory");
    std::fs::write(root.join("one.txt"), b"one").expect("write first file");
    std::fs::write(root.join("two.txt"), b"two").expect("write second file");

    let error = validate_share_limits(&root, None, NonZeroU64::new(5))
        .expect_err("total size limit should reject the share");

    assert!(error.to_string().contains("more than 5 bytes"));
    assert!(error.to_string().contains("max-total-size"));
}

#[test]
fn validate_imported_share_totals_rechecks_actual_limits() {
    let error = validate_imported_share_totals(2, 7, NonZeroU64::new(1), NonZeroU64::new(6))
        .expect_err("imported totals beyond either limit must fail");
    assert!(error.to_string().contains("files"));

    validate_imported_share_totals(1, 6, NonZeroU64::new(1), NonZeroU64::new(6))
        .expect("imported totals at both limits should pass");
}

#[test]
fn validate_share_path_rejects_empty_directory() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let empty = temp_dir.path().join("empty-share");
    std::fs::create_dir_all(&empty).expect("create empty directory");

    let error =
        validate_share_path(&empty, false).expect_err("empty directory should be rejected");

    assert!(error.to_string().contains("empty directory"));
    assert!(error.to_string().contains("empty-share"));
}

#[test]
fn validate_share_path_rejects_empty_nested_directory() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let root = temp_dir.path().join("share");
    let empty_nested = root.join("empty");
    std::fs::create_dir_all(&empty_nested).expect("create empty nested directory");
    std::fs::write(root.join("file.txt"), b"content").expect("write source file");

    let error = validate_share_path(&root, false)
        .expect_err("empty nested directory should be rejected");

    assert!(error.to_string().contains("empty directory"));
    assert!(error.to_string().contains("empty"));
}
