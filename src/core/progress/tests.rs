use super::{
    CompletionStatus, ProviderProgressTracker, SenderProgressReporter, SenderTransferStatus,
    TransferEventEmitter, TransferId,
};
use crate::core::events::{
    EventEmitter, Role, TransferError, TransferErrorCode, TransferEvent, TransferEventData,
    TransferPhase,
};
use crate::core::types::EntryType;
use iroh_blobs::provider::{
    TransferStats,
    events::{RequestUpdate, TransferAborted, TransferCompleted, TransferProgress},
};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

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

#[test]
fn lifecycle_diagnostic_does_not_log_failed_payload() {
    const CAPTURE_CHILD_ENV: &str = "SENDMER_PRIVACY_CAPTURE_CHILD";

    if std::env::var_os(CAPTURE_CHILD_ENV).is_none() {
        // Run capture in a fresh process so parallel tests cannot change the tracing callsite cache.
        let mut child = Command::new(std::env::current_exe().expect("test executable path"))
            .args([
                "--exact",
                "core::progress::tests::lifecycle_diagnostic_does_not_log_failed_payload",
                "--nocapture",
            ])
            .env(CAPTURE_CHILD_ENV, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start isolated privacy diagnostic test");
        let deadline = Instant::now() + Duration::from_secs(30);

        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    let output = child
                        .wait_with_output()
                        .expect("collect isolated privacy diagnostic test output");
                    assert!(
                        output.status.success(),
                        "isolated privacy diagnostic test failed"
                    );
                    return;
                }
                Ok(None) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("isolated privacy diagnostic test exceeded 30 seconds");
                }
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("could not inspect isolated privacy diagnostic test status");
                }
            }
        }
    }

    let logs = Arc::new(StdMutex::new(Vec::new()));
    let writer_logs = Arc::clone(&logs);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(move || TestLogWriter(Arc::clone(&writer_logs)))
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let emitter = TransferEventEmitter::new(None, Role::Receiver);
        emitter.emit_failed(TransferError::new(
            TransferErrorCode::Internal,
            TransferPhase::Preparing,
            false,
            r"C:\private\secret.txt",
        ));
    });

    let output = String::from_utf8(logs.lock().expect("log buffer lock").clone())
        .expect("tracing output should be UTF-8");
    assert!(output.contains("event_kind=\"failed\""));
    assert!(!output.contains(r"C:\private\secret.txt"));
}

/// Capture formatted tracing output for privacy assertions in the isolated child process.
struct TestLogWriter(Arc<StdMutex<Vec<u8>>>);

impl std::io::Write for TestLogWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("log buffer lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn started_emitter(sink: Arc<RecordingEmitter>, role: Role) -> TransferEventEmitter {
    let emitter = TransferEventEmitter::new(Some(sink), role);
    emitter.emit_started(TransferPhase::Transferring);
    emitter
}

#[test]
fn transfer_event_emitter_filters_unsafe_file_names() {
    let sink = Arc::new(RecordingEmitter::default());
    let emitter = started_emitter(sink.clone(), Role::Receiver);

    emitter.emit_file_names(vec!["safe/path.txt".to_owned()]);
    emitter.emit_file_names(vec!["../secret.txt".to_owned(), "C:/secret.txt".to_owned()]);
    emitter.emit_completed();

    let events = sink.events();
    assert_eq!(
        events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        events
            .iter()
            .filter_map(|event| match &event.event {
                TransferEventData::FileNames { file_names } => Some(file_names.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        vec![vec!["safe/path.txt".to_owned()]]
    );
}

#[test]
fn receiver_progress_clamps_and_never_regresses() {
    let emitter = Arc::new(RecordingEmitter::default());
    let session = started_emitter(emitter.clone(), Role::Receiver);
    let mut reporter = super::ReceiverProgressReporter::new(session, 100);
    reporter.emit_initial_progress();

    reporter.tracker.last_emit = Instant::now() - Duration::from_millis(201);
    reporter.on_progress(60);
    reporter.tracker.last_emit = Instant::now() - Duration::from_millis(201);
    reporter.on_progress(20);
    reporter.tracker.last_emit = Instant::now() - Duration::from_millis(201);
    reporter.on_progress(120);

    let progress = emitter
        .events()
        .into_iter()
        .filter_map(|event| match event.event {
            TransferEventData::Progress { processed, .. } => Some(processed),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(progress, vec![0, 60, 60, 100]);
}

#[test]
fn file_transfer_completes_after_quiet_period() {
    let mut tracker = ProviderProgressTracker::new(EntryType::File);
    let id = TransferId::new(1, 1);

    tracker.on_request_started(id, 128);
    assert!(matches!(
        tracker.on_request_completed(id),
        CompletionStatus::MoreRequestsArrivingSoon
    ));
    assert!(matches!(
        tracker.evaluate_completion(),
        CompletionStatus::MoreRequestsArrivingSoon
    ));

    sleep(tracker.completion_quiet_period());

    assert!(matches!(
        tracker.evaluate_completion(),
        CompletionStatus::Completed
    ));
    assert!(matches!(
        tracker.evaluate_completion(),
        CompletionStatus::InProgress
    ));
}

#[test]
fn directory_metadata_only_does_not_complete() {
    let mut tracker = ProviderProgressTracker::new(EntryType::Directory);
    let id = TransferId::new(2, 1);

    tracker.on_request_started(id, 64);
    assert!(matches!(
        tracker.on_request_completed(id),
        CompletionStatus::InProgress
    ));

    sleep(tracker.completion_quiet_period());

    assert!(matches!(
        tracker.evaluate_completion(),
        CompletionStatus::InProgress
    ));
}

#[test]
fn directory_transfer_waits_for_second_request() {
    let mut tracker = ProviderProgressTracker::new(EntryType::Directory);
    let first = TransferId::new(3, 1);
    let second = TransferId::new(3, 2);

    tracker.on_request_started(first, 256);
    assert!(matches!(
        tracker.on_request_completed(first),
        CompletionStatus::InProgress
    ));

    tracker.on_request_started(second, 256);
    assert!(matches!(
        tracker.on_request_completed(second),
        CompletionStatus::MoreRequestsArrivingSoon
    ));

    sleep(tracker.completion_quiet_period());

    assert!(matches!(
        tracker.evaluate_completion(),
        CompletionStatus::Completed
    ));
}

#[test]
fn aborted_request_does_not_trigger_completion() {
    let mut tracker = ProviderProgressTracker::new(EntryType::File);
    let id = TransferId::new(4, 1);

    tracker.on_request_started(id, 512);
    assert!(tracker.on_request_aborted(id));

    sleep(tracker.completion_quiet_period());

    assert!(matches!(
        tracker.evaluate_completion(),
        CompletionStatus::InProgress
    ));
}

#[test]
fn provider_progress_clamps_remote_offset_to_declared_total() {
    let mut tracker = ProviderProgressTracker::new(EntryType::File);
    let id = TransferId::new(5, 1);
    tracker.on_request_started(id, 100);
    sleep(Duration::from_millis(260));

    let (processed, total, speed) = tracker
        .on_progress(id, 250)
        .expect("progress should be emitted after throttle interval");
    assert_eq!(processed, 100);
    assert_eq!(total, 100);
    assert!(speed.is_finite());
    assert!(speed >= 0.0);
}

#[test]
fn provider_progress_never_regresses_after_a_later_remote_offset() {
    let mut tracker = ProviderProgressTracker::new(EntryType::File);
    let id = TransferId::new(6, 1);
    tracker.on_request_started(id, 100);
    sleep(Duration::from_millis(260));
    let first = tracker.on_progress(id, 80).expect("first progress");
    sleep(Duration::from_millis(260));
    let second = tracker.on_progress(id, 10).expect("second progress");

    assert_eq!(first.0, 80);
    assert_eq!(second.0, 80);
}

#[tokio::test]
async fn sender_progress_reporter_keeps_session_open_after_request_completes() {
    let sink = Arc::new(RecordingEmitter::default());
    let (status_tx, _status_rx) = tokio::sync::watch::channel(SenderTransferStatus::Idle);
    let session = started_emitter(sink.clone(), Role::Sender);
    let reporter = SenderProgressReporter::new(session, EntryType::File, status_tx);
    let id = TransferId::new(10, 1);

    reporter.on_request_received(id, 128).await;
    reporter
        .on_request_update(
            id,
            RequestUpdate::Completed(TransferCompleted {
                stats: transfer_stats(128),
            }),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(550)).await;

    let events = sink.events();
    assert!(matches!(
        events.first(),
        Some(event)
            if event.role == Role::Sender
                && matches!(&event.event, TransferEventData::Started)
    ));
    assert!(!events.iter().any(|event| event.event.is_terminal()));
}

#[tokio::test]
async fn sender_progress_reporter_keeps_session_open_after_request_abort() {
    let sink = Arc::new(RecordingEmitter::default());
    let (status_tx, mut status_rx) = tokio::sync::watch::channel(SenderTransferStatus::Idle);
    let session = started_emitter(sink.clone(), Role::Sender);
    let reporter = SenderProgressReporter::new(session, EntryType::File, status_tx);
    let id = TransferId::new(11, 1);

    reporter.on_request_received(id, 128).await;
    reporter
        .on_request_update(
            id,
            RequestUpdate::Aborted(TransferAborted {
                stats: transfer_stats(0),
            }),
        )
        .await;

    assert_eq!(
        *status_rx.borrow_and_update(),
        SenderTransferStatus::Started
    );

    let events = sink.events();
    assert!(!events.iter().any(|event| event.event.is_terminal()));
}

#[tokio::test]
async fn sender_progress_does_not_overwrite_expired_status() {
    let sink = Arc::new(RecordingEmitter::default());
    let (status_tx, status_rx) = tokio::sync::watch::channel(SenderTransferStatus::Idle);
    let session = started_emitter(sink, Role::Sender);
    let reporter = SenderProgressReporter::new(session, EntryType::File, status_tx.clone());
    status_tx
        .send(SenderTransferStatus::Expired)
        .expect("status receiver");

    reporter
        .on_request_received(TransferId::new(12, 1), 128)
        .await;

    assert_eq!(*status_rx.borrow(), SenderTransferStatus::Expired);
}

#[tokio::test]
async fn sender_session_aggregates_multiple_receivers_until_explicit_close() {
    let sink = Arc::new(RecordingEmitter::default());
    let (status_tx, _status_rx) = tokio::sync::watch::channel(SenderTransferStatus::Idle);
    let session = started_emitter(sink.clone(), Role::Sender);
    let reporter = SenderProgressReporter::new(session.clone(), EntryType::File, status_tx);
    let first = TransferId::new(21, 1);
    let second = TransferId::new(22, 1);

    reporter.on_request_received(first, 128).await;
    reporter
        .on_request_update(
            first,
            RequestUpdate::Progress(TransferProgress { end_offset: 32 }),
        )
        .await;
    reporter
        .on_request_update(
            first,
            RequestUpdate::Aborted(TransferAborted {
                stats: transfer_stats(32),
            }),
        )
        .await;
    reporter.on_request_received(second, 128).await;
    reporter
        .on_request_update(
            second,
            RequestUpdate::Progress(TransferProgress { end_offset: 96 }),
        )
        .await;
    reporter
        .on_request_update(
            second,
            RequestUpdate::Completed(TransferCompleted {
                stats: transfer_stats(128),
            }),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(550)).await;

    assert!(!sink.events().iter().any(|event| event.event.is_terminal()));
    session.emit_completed();

    let events = sink.events();
    let session_id = events.first().expect("started event").session_id.clone();
    assert!(events.iter().all(|event| event.session_id == session_id));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.is_terminal())
            .count(),
        1
    );
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Completed)
    ));
}

#[test]
fn transfer_event_emitter_orders_concurrent_callbacks() {
    let sink = Arc::new(RecordingEmitter::default());
    let emitter = started_emitter(sink.clone(), Role::Sender);
    let threads = (0..8)
        .map(|value| {
            let emitter = emitter.clone();
            std::thread::spawn(move || emitter.emit_progress(value, 8, 1.0))
        })
        .collect::<Vec<_>>();

    for thread in threads {
        thread.join().expect("progress callback");
    }
    emitter.emit_completed();

    let events = sink.events();
    let session_id = events.first().expect("started event").session_id.clone();
    assert_eq!(events.len(), 10);
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.session_id, session_id);
        assert_eq!(event.sequence, u64::try_from(index + 1).expect("sequence"));
    }
    assert!(matches!(events[0].event, TransferEventData::Started));
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Completed)
    ));
}

#[test]
fn transfer_event_emitter_allows_only_one_terminal_event() {
    let sink = Arc::new(RecordingEmitter::default());
    let emitter = started_emitter(sink.clone(), Role::Receiver);

    emitter.emit_completed();
    emitter.emit_cancelled();
    emitter.emit_failed(TransferError::new(
        TransferErrorCode::Internal,
        TransferPhase::Finalizing,
        false,
        "late failure",
    ));
    emitter.emit_progress(1, 1, 1.0);

    let events = sink.events();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.is_terminal())
            .count(),
        1
    );
    assert!(matches!(events[1].event, TransferEventData::Completed));
}

fn transfer_stats(payload_bytes_sent: u64) -> Box<TransferStats> {
    Box::new(TransferStats {
        payload_bytes_sent,
        other_bytes_sent: 0,
        other_bytes_read: 0,
        duration: Duration::from_millis(100),
    })
}
