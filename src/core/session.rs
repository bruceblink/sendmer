use crate::core::events::{AppHandle, Role, TransferEvent};
use crate::core::progress::ProgressTracker;

pub struct TransferSession {
    role: Role,
    emitter: AppHandle,
    progress: ProgressTracker,
    started: bool,
}

impl TransferSession {
    pub fn new(role: Role, emitter: AppHandle) -> Self {
        Self {
            role,
            emitter,
            progress: ProgressTracker::new(),
            started: false,
        }
    }

    pub fn start(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.emit(TransferEvent::Started { role: self.role });
    }

    pub fn set_total(&mut self, total: u64) {
        self.progress.set_total(total);
    }

    pub fn advance(&mut self, value: u64) {
        if let Some(snapshot) = self.progress.update(value) {
            self.emit(TransferEvent::Progress {
                role: self.role,
                processed: snapshot.current,
                total: snapshot.total,
                speed: snapshot.speed,
            });
        }
    }

    pub fn finish(&mut self) {
        self.emit(TransferEvent::Completed { role: self.role });
    }

    pub fn fail(&mut self, msg: String) {
        self.emit(TransferEvent::Failed {
            role: self.role,
            message: msg,
        });
    }

    pub fn emit_file_names(&self, names: Vec<String>) {
        self.emit(TransferEvent::FileNames {
            role: self.role,
            file_names: names,
        });
    }

    fn emit(&self, event: TransferEvent) {
        if let Some(handle) = &self.emitter {
            handle.emit(&event);
        }
    }
}