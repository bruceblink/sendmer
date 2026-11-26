use crate::core::events::{EventEmitter, TransferEvent};
use std::sync::Mutex;

#[derive(Default)]
pub(super) struct RecordingEmitter {
    events: Mutex<Vec<TransferEvent>>,
}

impl RecordingEmitter {
    pub(super) fn events(&self) -> Vec<TransferEvent> {
        self.events.lock().expect("events lock").clone()
    }
}

impl EventEmitter for RecordingEmitter {
    fn emit(&self, event: &TransferEvent) {
        self.events.lock().expect("events lock").push(event.clone());
    }
}
