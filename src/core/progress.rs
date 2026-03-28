use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::core::types::EntryType;

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

    pub fn set_total(&mut self, total: u64) {
        self.total = total;
    }

    pub fn update(&mut self, current: u64) -> Option<ProgressSnapshot> {
        self.current = current;

        if self.last_emit.elapsed() < Duration::from_millis(200) {
            return None;
        }

        self.last_emit = Instant::now();

        let elapsed = self.start.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            self.current as f64 / elapsed
        } else {
            0.0
        };

        Some(ProgressSnapshot {
            current: self.current,
            total: self.total,
            speed,
        })
    }
}

/// Transfer ID combining connection and request IDs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TransferId {
    pub connection: u64,
    pub request: u64,
}

impl TransferId {
    pub fn new(connection: u64, request: u64) -> Self {
        Self { connection, request }
    }
}

/// Information about an active transfer
#[derive(Debug)]
pub struct TransferInfo {
    pub start_time: Instant,
    pub total_size: u64,
    pub last_progress_emit: Instant,
}

/// Completion detection logic
#[derive(Debug)]
pub struct CompletionDetector {
    entry_type: EntryType,
}

impl CompletionDetector {
    pub fn new(entry_type: EntryType) -> Self {
        Self { entry_type }
    }

    pub fn min_required(&self) -> usize {
        match self.entry_type {
            EntryType::File => 1,
            EntryType::Directory => 2,
        }
    }

    pub fn is_complete(
        &self,
        completed: usize,
        active: usize,
        has_transfers: bool,
        transfer_states: &HashMap<TransferId, TransferInfo>,
        last_request_time: Instant,
    ) -> bool {
        let min_required = self.min_required();

        if completed < min_required || !has_transfers {
            return false;
        }

        if completed < active {
            return false;
        }

        // Check if there have been recent requests (within 500ms)
        if last_request_time.elapsed() < Duration::from_millis(500) {
            return false;
        }

        // Check if there are any active transfers
        transfer_states.is_empty()
    }
}

/// Provider-side progress tracker for managing multiple concurrent transfers
pub struct ProviderProgressTracker {
    transfer_states: HashMap<TransferId, TransferInfo>,
    completed_count: AtomicUsize,
    active_count: AtomicUsize,
    completion_detector: CompletionDetector,
    last_completion_check: Instant,
    progress_throttle: Duration,
}

impl ProviderProgressTracker {
    pub fn new(entry_type: EntryType) -> Self {
        Self {
            transfer_states: HashMap::new(),
            completed_count: AtomicUsize::new(0),
            active_count: AtomicUsize::new(0),
            completion_detector: CompletionDetector::new(entry_type),
            last_completion_check: Instant::now(),
            progress_throttle: Duration::from_millis(250), // Default throttle
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
        self.active_count.fetch_add(1, Ordering::SeqCst);
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
        let speed = if elapsed > 0.0 { processed as f64 / elapsed } else { 0.0 };

        Some((processed, total, speed))
    }

    /// Record that a request has completed, returning completion status
    pub fn on_request_completed(&mut self, id: TransferId) -> CompletionStatus {
        if self.transfer_states.remove(&id).is_some() {
            self.completed_count.fetch_add(1, Ordering::SeqCst);
            self.active_count.fetch_sub(1, Ordering::SeqCst);
        }

        // Check completion periodically, not on every event
        if self.last_completion_check.elapsed() < Duration::from_millis(100) {
            return CompletionStatus::InProgress;
        }

        self.last_completion_check = Instant::now();

        let completed = self.completed_count.load(Ordering::SeqCst);
        let active = self.active_count.load(Ordering::SeqCst);
        let has_transfers = !self.transfer_states.is_empty();
        let last_request_time = self.transfer_states.values()
            .map(|info| info.start_time)
            .max()
            .unwrap_or_else(Instant::now);

        if self.completion_detector.is_complete(
            completed,
            active,
            has_transfers,
            &self.transfer_states,
            last_request_time,
        ) {
            CompletionStatus::Completed
        } else if has_transfers {
            CompletionStatus::InProgress
        } else {
            CompletionStatus::MoreRequestsArrivingSoon
        }
    }

    /// Record that a request was aborted
    pub fn on_request_aborted(&mut self, id: TransferId) -> bool {
        if self.transfer_states.remove(&id).is_some() {
            self.active_count.fetch_sub(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Get current statistics
    pub fn stats(&self) -> ProgressStats {
        ProgressStats {
            active: self.active_count.load(Ordering::SeqCst),
            completed: self.completed_count.load(Ordering::SeqCst),
            total_transfers: self.transfer_states.len(),
        }
    }
}

/// Completion status after processing a request
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStatus {
    InProgress,
    Completed,
    MoreRequestsArrivingSoon,
}

/// Current progress statistics
#[derive(Debug, Clone)]
pub struct ProgressStats {
    pub active: usize,
    pub completed: usize,
    pub total_transfers: usize,
}