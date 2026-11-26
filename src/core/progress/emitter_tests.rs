use super::TransferEventEmitter;
use crate::core::events::{
    EventEmitter, Role, TransferError, TransferErrorCode, TransferEventData, TransferEventEnvelope,
    TransferEventStreamValidator, TransferPhase,
};
use std::sync::{Arc, Barrier, Mutex};

#[derive(Default)]
struct RecordingEmitter(Mutex<Vec<TransferEventEnvelope>>);

impl EventEmitter for RecordingEmitter {
    fn emit(&self, event: &TransferEventEnvelope) {
        self.0.lock().unwrap().push(event.clone());
    }
}

fn assert_valid_stream(sink: &RecordingEmitter) -> Vec<TransferEventEnvelope> {
    let events = sink.0.lock().unwrap().clone();
    let mut validator = TransferEventStreamValidator::new();
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, u64::try_from(index + 1).unwrap());
        validator.accept(event).unwrap();
    }
    assert!(validator.is_terminal());
    assert!(!validator.is_rejected());
    events
}

#[test]
fn emitter_rejects_invalid_inputs_without_consuming_sequence() {
    let sink = Arc::new(RecordingEmitter::default());
    let emitter = TransferEventEmitter::new(Some(sink.clone()), Role::Sender);
    emitter.emit_started(TransferPhase::Exporting);
    emitter.emit_progress(1, 2, 1.0);
    emitter.emit_completed();
    emitter.emit_started(TransferPhase::Preparing);
    emitter.emit_started(TransferPhase::Transferring);
    emitter.emit_progress(3, 2, 1.0);
    emitter.emit_progress(1, 2, f64::NAN);
    emitter.emit_progress(1, 2, -1.0);
    emitter.emit_file_names(vec!["../secret".into()]);
    emitter.emit_failed(TransferError::new(
        TransferErrorCode::Internal,
        TransferPhase::Exporting,
        false,
        "invalid sender phase",
    ));
    emitter.emit_progress(1, 2, 1.0);
    emitter.emit_cancelled();
    emitter.emit_completed();
    emitter.emit_progress(2, 2, 1.0);

    let events = assert_valid_stream(&sink);
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].phase, TransferPhase::Preparing);
    assert!(matches!(
        events[1].event,
        TransferEventData::Progress { .. }
    ));
    assert!(matches!(events[2].event, TransferEventData::Cancelled));
    assert_eq!(events[2].phase, TransferPhase::Transferring);
}

#[test]
fn concurrent_cancellation_failure_completion_and_telemetry_share_one_terminal() {
    // Repeat with both roles and contend for one session lock. Do not rely on
    // thread scheduling or assume which terminal wins.
    for role in [Role::Sender, Role::Receiver] {
        for _ in 0..16 {
            let sink = Arc::new(RecordingEmitter::default());
            let emitter = TransferEventEmitter::new(Some(sink.clone()), role);
            emitter.emit_started(TransferPhase::Preparing);
            let barrier = Arc::new(Barrier::new(5));

            std::thread::scope(|scope| {
                for action in 0..5 {
                    let emitter = emitter.clone();
                    let barrier = barrier.clone();
                    scope.spawn(move || {
                        barrier.wait();
                        match action {
                            0 => emitter.emit_progress(1, 2, 1.0),
                            1 => emitter.emit_file_names(vec!["safe.txt".into()]),
                            2 => emitter.emit_cancelled(),
                            3 => emitter.emit_internal_failure("failure"),
                            4 => emitter.emit_completed(),
                            _ => unreachable!(),
                        }
                    });
                }
            });

            emitter.emit_cancelled();
            emitter.emit_progress(2, 2, 1.0);
            let events = assert_valid_stream(&sink);
            assert_eq!(events.iter().filter(|e| e.event.is_terminal()).count(), 1);
            assert!(events.last().unwrap().event.is_terminal());
        }
    }
}
