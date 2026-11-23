use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::core::types::EntryType;

pub struct ProgressTracker {
    start: Instant,
    pub(super) last_emit: Instant,
    pub(super) current: u64,
    pub(super) total: u64,
}

pub struct ProgressSnapshot {
    pub current: u64,
    pub total: u64,
    pub speed: f64,
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
    pub last_processed: u64,
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
            last_processed: 0,
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

        // provider 的偏移量来自远端请求，统一限制在声明总量内，避免进度倒退或越界。
        let processed = offset.min(info.total_size).max(info.last_processed);
        info.last_processed = processed;
        let total = info.total_size;
        let elapsed = info.start_time.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            processed as f64 / elapsed
        } else {
            0.0
        };

        Some((processed, total, speed.max(0.0)))
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
