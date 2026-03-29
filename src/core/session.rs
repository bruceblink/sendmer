use crate::core::events::{AppHandle, Role};
use crate::core::progress::{ProgressTracker, TransferEventEmitter};

pub struct TransferSession {
    emitter: TransferEventEmitter,
    progress: ProgressTracker,
    started: bool,
}

impl TransferSession {
    pub fn new(role: Role, emitter: AppHandle) -> Self {
        Self {
            emitter: TransferEventEmitter::new(emitter, role),
            progress: ProgressTracker::new(),
            started: false,
        }
    }

    pub fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.emitter.emit_started();
    }

    pub const fn set_total(&mut self, total: u64) {
        self.progress.set_total(total);
    }

    pub fn advance(&mut self, value: u64) {
        if let Some(snapshot) = self.progress.update(value) {
            self.emitter
                .emit_progress(snapshot.current, snapshot.total, snapshot.speed);
        }
    }

    pub fn finish(&mut self) {
        self.emitter.emit_completed();
    }

    pub fn fail(&mut self, msg: String) {
        self.emitter.emit_failed(msg);
    }

    pub fn emit_file_names(&self, names: Vec<String>) {
        self.emitter.emit_file_names(names);
    }
}
