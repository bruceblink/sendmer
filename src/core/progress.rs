use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::core::events::{
    AppHandle, Role, TransferError, TransferEventData, TransferEventEnvelope, TransferPhase,
    TransferSessionId, emit_event,
};
use crate::core::types::EntryType;
use tokio::sync::{Mutex, watch};

pub struct ProgressTracker {
    start: Instant,
    last_emit: Instant,
    current: u64,
    total: u64,
}

pub struct ProgressSnapshot {
    pub current: u64,
    pub total: u64,
    pub speed: f64,
}

#[derive(Clone)]
pub struct TransferEventEmitter {
    app_handle: AppHandle,
    role: Role,
    state: Arc<StdMutex<TransferEventState>>,
}

struct TransferEventState {
    session_id: TransferSessionId,
    next_sequence: u64,
    started: bool,
    terminal: bool,
    current_phase: TransferPhase,
}

impl TransferEventEmitter {
    pub fn new(app_handle: AppHandle, role: Role) -> Self {
        Self {
            app_handle,
            role,
            state: Arc::new(StdMutex::new(TransferEventState {
                session_id: TransferSessionId::new(),
                next_sequence: 1,
                started: false,
                terminal: false,
                current_phase: TransferPhase::Preparing,
            })),
        }
    }

    /// Start one session before any progress or terminal event is emitted.
    pub fn emit_started(&self, phase: TransferPhase) {
        self.emit_data(phase, TransferEventData::Started);
    }

    pub fn emit_progress(&self, processed: u64, total: u64, speed: f64) {
        self.emit_data(
            TransferPhase::Transferring,
            TransferEventData::Progress {
                processed,
                total,
                speed_bytes_per_sec: speed,
            },
        );
    }

    pub fn emit_completed(&self) {
        self.emit_data(TransferPhase::Finalizing, TransferEventData::Completed);
    }

    pub fn emit_failed(&self, error: TransferError) {
        self.emit_data(error.phase, TransferEventData::Failed { error });
    }

    /// Emit a conservative failure until the caller has a more specific error mapping.
    pub fn emit_internal_failure(&self, message: impl Into<String>) {
        self.emit_failed(TransferError::new(
            crate::core::events::TransferErrorCode::Internal,
            self.current_phase(),
            false,
            message,
        ));
    }

    pub fn emit_cancelled(&self) {
        let phase = self.current_phase();
        self.emit_data(phase, TransferEventData::Cancelled);
    }

    pub fn emit_file_names(&self, file_names: Vec<String>) {
        self.emit_data(
            TransferPhase::Metadata,
            TransferEventData::FileNames { file_names },
        );
    }

    fn current_phase(&self) -> TransferPhase {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_phase
    }

    /// Assign sequence and timestamp while holding the session lock through emission.
    ///
    /// Keeping both operations under one lock ensures concurrent provider callbacks are
    /// observed in the same order as their sequence numbers.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the state lock deliberately preserves emission order"
    )]
    fn emit_data(&self, phase: TransferPhase, event: TransferEventData) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let is_started = matches!(event, TransferEventData::Started);
        if state.terminal || (is_started && state.started) || (!is_started && !state.started) {
            tracing::debug!(
                session_id = %state.session_id,
                ?phase,
                ?event,
                "ignored transfer event that violates session lifecycle"
            );
            return false;
        }
        let Some(next_sequence) = state.next_sequence.checked_add(1) else {
            tracing::warn!(
                session_id = %state.session_id,
                "transfer event sequence exhausted"
            );
            state.terminal = true;
            return false;
        };
        let envelope = TransferEventEnvelope::new(
            state.session_id.clone(),
            state.next_sequence,
            unix_timestamp_ms(),
            self.role,
            phase,
            event,
        );
        state.next_sequence = next_sequence;
        state.started |= is_started;
        state.terminal |= envelope.event.is_terminal();
        state.current_phase = phase;
        emit_event(&self.app_handle, &envelope);
        true
    }
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

impl ProgressTracker {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last_emit: now,
            current: 0,
            total: 0,
        }
    }

    pub const fn set_total(&mut self, total: u64) {
        self.total = total;
    }

    pub fn snapshot(&self) -> ProgressSnapshot {
        let elapsed = self.start.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            self.current as f64 / elapsed
        } else {
            0.0
        };

        ProgressSnapshot {
            current: self.current,
            total: self.total,
            speed,
        }
    }

    pub fn update(&mut self, current: u64) -> Option<ProgressSnapshot> {
        self.current = current;

        if self.last_emit.elapsed() < Duration::from_millis(200) {
            return None;
        }

        self.last_emit = Instant::now();

        Some(self.snapshot())
    }
}

/// Transfer ID combining connection and request IDs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId {
    pub connection: u64,
    pub request: u64,
}

impl TransferId {
    pub const fn new(connection: u64, request: u64) -> Self {
        Self {
            connection,
            request,
        }
    }
}

/// Information about an active transfer
#[derive(Debug)]
pub struct TransferInfo {
    pub start_time: Instant,
    pub total_size: u64,
    pub last_progress_emit: Instant,
}

/// Provider-side progress tracker for managing multiple concurrent transfers
pub struct ProviderProgressTracker {
    transfer_states: HashMap<TransferId, TransferInfo>,
    active_requests: usize,
    completed_requests: usize,
    has_any_transfer: bool,
    last_request_time: Option<Instant>,
    entry_type: EntryType,
    progress_throttle: Duration,
    completion_quiet_period: Duration,
    completed_emitted: bool,
}

impl ProviderProgressTracker {
    pub fn new(entry_type: EntryType) -> Self {
        Self {
            transfer_states: HashMap::new(),
            active_requests: 0,
            completed_requests: 0,
            has_any_transfer: false,
            last_request_time: None,
            entry_type,
            progress_throttle: Duration::from_millis(250),
            completion_quiet_period: Duration::from_millis(500),
            completed_emitted: false,
        }
    }

    /// Record that a request has started
    pub fn on_request_started(&mut self, id: TransferId, total_size: u64) {
        let info = TransferInfo {
            start_time: Instant::now(),
            total_size,
            last_progress_emit: Instant::now(),
        };
        self.transfer_states.insert(id, info);
        self.active_requests += 1;
        self.has_any_transfer = true;
        self.last_request_time = Some(Instant::now());
    }

    /// Update progress for a transfer, potentially returning progress event data
    pub fn on_progress(&mut self, id: TransferId, offset: u64) -> Option<(u64, u64, f64)> {
        let info = self.transfer_states.get_mut(&id)?;

        // Throttle progress emissions
        if info.last_progress_emit.elapsed() < self.progress_throttle {
            return None;
        }

        info.last_progress_emit = Instant::now();

        let processed = offset;
        let total = info.total_size;
        let elapsed = info.start_time.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            processed as f64 / elapsed
        } else {
            0.0
        };

        Some((processed, total, speed))
    }

    /// Record that a request has completed.
    ///
    /// Returns the current completion status. When `MoreRequestsArrivingSoon`
    /// is returned, the caller should wait for the quiet period and re-check.
    pub fn on_request_completed(&mut self, id: TransferId) -> CompletionStatus {
        if self.transfer_states.remove(&id).is_some() {
            self.completed_requests += 1;
            self.active_requests = self.active_requests.saturating_sub(1);
        }

        if !self.can_finish_once_quiet() {
            CompletionStatus::InProgress
        } else {
            CompletionStatus::MoreRequestsArrivingSoon
        }
    }

    /// Record that a request was aborted
    pub fn on_request_aborted(&mut self, id: TransferId) -> bool {
        if self.transfer_states.remove(&id).is_some() {
            self.active_requests = self.active_requests.saturating_sub(1);
            true
        } else {
            false
        }
    }

    /// Evaluate whether completion may now be emitted after a quiet period.
    pub fn evaluate_completion(&mut self) -> CompletionStatus {
        if self.completed_emitted {
            return CompletionStatus::InProgress;
        }

        if !self.can_finish_once_quiet() {
            return CompletionStatus::InProgress;
        }

        let Some(last_request_time) = self.last_request_time else {
            return CompletionStatus::InProgress;
        };

        if last_request_time.elapsed() < self.completion_quiet_period {
            return CompletionStatus::MoreRequestsArrivingSoon;
        }

        if self.is_complete(last_request_time) {
            self.completed_emitted = true;
            CompletionStatus::Completed
        } else {
            CompletionStatus::InProgress
        }
    }

    pub const fn completion_quiet_period(&self) -> Duration {
        self.completion_quiet_period
    }

    const fn can_finish_once_quiet(&self) -> bool {
        !self.completed_emitted
            && self.has_any_transfer
            && self.completed_requests >= self.entry_type.min_required_transfers()
            && self.completed_requests >= self.active_requests
    }

    fn is_complete(&self, last_request_time: Instant) -> bool {
        if self.completed_requests < self.entry_type.min_required_transfers()
            || !self.has_any_transfer
            || self.completed_requests < self.active_requests
        {
            return false;
        }

        if last_request_time.elapsed() < self.completion_quiet_period {
            return false;
        }

        self.transfer_states.is_empty()
    }
}

/// Completion status after processing a request
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    InProgress,
    Completed,
    MoreRequestsArrivingSoon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderTransferStatus {
    Idle,
    Started,
    Completed,
    Aborted,
    /// The sender's configured fixed lifetime elapsed and the share was closed.
    Expired,
}

#[derive(Clone)]
pub struct SenderProgressReporter {
    emitter: TransferEventEmitter,
    state: Arc<Mutex<SenderProgressState>>,
    status_tx: watch::Sender<SenderTransferStatus>,
}

struct SenderProgressState {
    tracker: ProviderProgressTracker,
}

impl SenderProgressReporter {
    pub fn new(
        emitter: TransferEventEmitter,
        entry_type: EntryType,
        status_tx: watch::Sender<SenderTransferStatus>,
    ) -> Self {
        Self {
            emitter,
            state: Arc::new(Mutex::new(SenderProgressState {
                tracker: ProviderProgressTracker::new(entry_type),
            })),
            status_tx,
        }
    }

    /// Publish telemetry without allowing a late provider callback to overwrite
    /// the sender's terminal `Expired` lifecycle state.
    fn publish_status(&self, status: SenderTransferStatus) {
        let _ = self.status_tx.send_if_modified(|current| {
            if *current == SenderTransferStatus::Expired || *current == status {
                false
            } else {
                *current = status;
                true
            }
        });
    }

    pub async fn on_request_received(&self, transfer_id: TransferId, total_file_size: u64) {
        let mut state = self.state.lock().await;
        state
            .tracker
            .on_request_started(transfer_id, total_file_size);
        drop(state);
        self.publish_status(SenderTransferStatus::Started);
    }

    pub async fn on_request_update(
        &self,
        transfer_id: TransferId,
        update: iroh_blobs::provider::events::RequestUpdate,
    ) {
        match update {
            iroh_blobs::provider::events::RequestUpdate::Started(_) => {}
            iroh_blobs::provider::events::RequestUpdate::Progress(m) => {
                let mut state = self.state.lock().await;
                if let Some((processed, total, speed)) =
                    state.tracker.on_progress(transfer_id, m.end_offset)
                {
                    self.emitter.emit_progress(processed, total, speed);
                }
            }
            iroh_blobs::provider::events::RequestUpdate::Completed(_) => {
                let quiet_period = {
                    let mut state = self.state.lock().await;
                    match state.tracker.on_request_completed(transfer_id) {
                        CompletionStatus::Completed => {
                            self.publish_status(SenderTransferStatus::Completed);
                            None
                        }
                        CompletionStatus::InProgress => None,
                        CompletionStatus::MoreRequestsArrivingSoon => {
                            Some(state.tracker.completion_quiet_period())
                        }
                    }
                };

                if let Some(quiet_period) = quiet_period {
                    tokio::time::sleep(quiet_period).await;

                    let mut state = self.state.lock().await;
                    if matches!(
                        state.tracker.evaluate_completion(),
                        CompletionStatus::Completed
                    ) {
                        self.publish_status(SenderTransferStatus::Completed);
                    }
                }
            }
            iroh_blobs::provider::events::RequestUpdate::Aborted(_) => {
                let mut state = self.state.lock().await;
                state.tracker.on_request_aborted(transfer_id);
            }
        }
    }
}

pub struct ReceiverProgressReporter {
    tracker: ProgressTracker,
    emitter: TransferEventEmitter,
}

impl ReceiverProgressReporter {
    pub fn new(emitter: TransferEventEmitter, total: u64) -> Self {
        let mut tracker = ProgressTracker::new();
        tracker.set_total(total);
        Self { tracker, emitter }
    }

    pub fn emit_initial_progress(&self) {
        self.emitter.emit_progress(0, self.tracker.total, 0.0);
    }

    pub fn on_progress(&mut self, current: u64) {
        let current = current.min(self.tracker.total).max(self.tracker.current);
        if let Some(snapshot) = self.tracker.update(current) {
            self.emitter
                .emit_progress(snapshot.current, snapshot.total, snapshot.speed);
        }
    }

    pub fn emit_completed_progress(&mut self) {
        self.tracker.current = self.tracker.total;
        let snapshot = self.tracker.snapshot();
        self.emitter
            .emit_progress(snapshot.current, snapshot.total, snapshot.speed);
    }
}

#[cfg(test)]
mod tests {
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

    fn started_emitter(sink: Arc<RecordingEmitter>, role: Role) -> TransferEventEmitter {
        let emitter = TransferEventEmitter::new(Some(sink), role);
        emitter.emit_started(TransferPhase::Transferring);
        emitter
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
}
