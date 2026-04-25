use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::core::events::{AppHandle, Role, TransferEvent, emit_event};
use crate::core::types::EntryType;
use tokio::sync::Mutex;

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
}

impl TransferEventEmitter {
    pub fn new(app_handle: AppHandle, role: Role) -> Self {
        Self { app_handle, role }
    }

    pub fn emit_started(&self) {
        emit_event(
            &self.app_handle,
            &TransferEvent::Started { role: self.role },
        );
    }

    pub fn emit_progress(&self, processed: u64, total: u64, speed: f64) {
        emit_event(
            &self.app_handle,
            &TransferEvent::Progress {
                role: self.role,
                processed,
                total,
                speed,
            },
        );
    }

    pub fn emit_completed(&self) {
        emit_event(
            &self.app_handle,
            &TransferEvent::Completed { role: self.role },
        );
    }

    pub fn emit_failed(&self, message: impl Into<String>) {
        emit_event(
            &self.app_handle,
            &TransferEvent::Failed {
                role: self.role,
                message: message.into(),
            },
        );
    }

    pub fn emit_file_names(&self, file_names: Vec<String>) {
        emit_event(
            &self.app_handle,
            &TransferEvent::FileNames {
                role: self.role,
                file_names,
            },
        );
    }
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

#[derive(Clone)]
pub struct SenderProgressReporter {
    emitter: TransferEventEmitter,
    state: Arc<Mutex<SenderProgressState>>,
}

struct SenderProgressState {
    tracker: ProviderProgressTracker,
    has_emitted_started: bool,
}

impl SenderProgressReporter {
    pub fn new(app_handle: AppHandle, entry_type: EntryType) -> Self {
        Self {
            emitter: TransferEventEmitter::new(app_handle, Role::Sender),
            state: Arc::new(Mutex::new(SenderProgressState {
                tracker: ProviderProgressTracker::new(entry_type),
                has_emitted_started: false,
            })),
        }
    }

    pub async fn on_request_received(&self, transfer_id: TransferId, total_file_size: u64) {
        let mut state = self.state.lock().await;
        state
            .tracker
            .on_request_started(transfer_id, total_file_size);
        if !state.has_emitted_started {
            self.emitter.emit_started();
            state.has_emitted_started = true;
        }
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
                            self.emitter.emit_completed();
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
                        self.emitter.emit_completed();
                    }
                }
            }
            iroh_blobs::provider::events::RequestUpdate::Aborted(_) => {
                let should_emit_failed = {
                    let mut state = self.state.lock().await;
                    state.tracker.on_request_aborted(transfer_id)
                };

                if should_emit_failed {
                    self.emitter.emit_failed("transfer aborted");
                }
            }
        }
    }
}

pub struct ReceiverProgressReporter {
    tracker: ProgressTracker,
    emitter: TransferEventEmitter,
}

impl ReceiverProgressReporter {
    pub fn new(app_handle: AppHandle, total: u64) -> Self {
        let mut tracker = ProgressTracker::new();
        tracker.set_total(total);
        Self {
            tracker,
            emitter: TransferEventEmitter::new(app_handle, Role::Receiver),
        }
    }

    pub fn emit_initial_progress(&self) {
        self.emitter.emit_progress(0, self.tracker.total, 0.0);
    }

    pub fn on_progress(&mut self, current: u64) {
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

    pub fn emit_failed(&self, message: impl Into<String>) {
        self.emitter.emit_failed(message);
    }
}

#[cfg(test)]
mod tests {
    use super::{CompletionStatus, ProviderProgressTracker, SenderProgressReporter, TransferId};
    use crate::core::events::{EventEmitter, Role, TransferEvent};
    use crate::core::types::EntryType;
    use iroh_blobs::provider::{TransferStats, events::TransferCompleted};
    use std::sync::{Arc, Mutex as StdMutex};
    use std::thread::sleep;
    use std::time::Duration;

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
    async fn sender_progress_reporter_emits_started_and_completed() {
        let sink = Arc::new(RecordingEmitter::default());
        let reporter = SenderProgressReporter::new(Some(sink.clone()), EntryType::File);
        let id = TransferId::new(10, 1);

        reporter.on_request_received(id, 128).await;
        reporter
            .on_request_update(
                id,
                iroh_blobs::provider::events::RequestUpdate::Completed(TransferCompleted {
                    stats: Box::new(TransferStats {
                        payload_bytes_sent: 128,
                        other_bytes_sent: 0,
                        other_bytes_read: 0,
                        duration: Duration::from_millis(100),
                    }),
                }),
            )
            .await;
        tokio::time::sleep(Duration::from_millis(550)).await;

        let events = sink.events();
        assert!(matches!(
            events.first(),
            Some(TransferEvent::Started { role: Role::Sender })
        ));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, TransferEvent::Completed { role: Role::Sender }))
        );
    }
}
