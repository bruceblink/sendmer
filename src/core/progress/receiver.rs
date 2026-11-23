use super::{ProgressTracker, TransferEventEmitter};

pub struct ReceiverProgressReporter {
    pub(super) tracker: ProgressTracker,
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
