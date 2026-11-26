use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::events::{
    AppHandle, EventStreamState, Role, TransferError, TransferEventData, TransferEventEnvelope,
    TransferPhase, TransferSessionId, emit_event,
};

#[derive(Clone)]
pub struct TransferEventEmitter {
    app_handle: AppHandle,
    role: Role,
    state: Arc<StdMutex<TransferEventState>>,
}

struct TransferEventState {
    session_id: TransferSessionId,
    stream: EventStreamState,
}

impl TransferEventEmitter {
    pub fn new(app_handle: AppHandle, role: Role) -> Self {
        Self {
            app_handle,
            role,
            state: Arc::new(StdMutex::new(TransferEventState {
                session_id: TransferSessionId::new(),
                stream: EventStreamState::new(),
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
        let message = message.into();
        self.emit_with(move |phase| {
            let error = TransferError::new(
                crate::core::events::TransferErrorCode::Internal,
                phase,
                false,
                message,
            );
            (phase, TransferEventData::Failed { error })
        });
    }

    pub fn emit_cancelled(&self) {
        self.emit_with(|phase| (phase, TransferEventData::Cancelled));
    }

    /// Emit relative logical names without exposing absolute or traversing paths.
    pub fn emit_file_names(&self, file_names: Vec<String>) {
        self.emit_data(
            TransferPhase::Metadata,
            TransferEventData::FileNames { file_names },
        );
    }

    fn emit_data(&self, phase: TransferPhase, event: TransferEventData) -> bool {
        self.emit_with(move |_| (phase, event))
    }

    /// Assign sequence and timestamp while holding the session lock through emission.
    ///
    /// Keeping both operations under one lock ensures concurrent provider callbacks are
    /// observed in the same order as their sequence numbers.
    #[allow(
        clippy::significant_drop_tightening,
        reason = "the state lock deliberately preserves emission order"
    )]
    fn emit_with(
        &self,
        build: impl FnOnce(TransferPhase) -> (TransferPhase, TransferEventData),
    ) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let (phase, event) = build(state.stream.current_phase());
        let event_kind = match &event {
            TransferEventData::Started => "started",
            TransferEventData::Progress { .. } => "progress",
            TransferEventData::FileNames { .. } => "file_names",
            TransferEventData::Completed => "completed",
            TransferEventData::Failed { .. } => "failed",
            TransferEventData::Cancelled => "cancelled",
        };
        let envelope = TransferEventEnvelope::new(
            state.session_id.clone(),
            state.stream.next_sequence(),
            unix_timestamp_ms(),
            self.role,
            phase,
            event,
        );
        if let Err(error) = state.stream.accept(&envelope) {
            if matches!(
                error,
                crate::core::events::TransferEventStreamError::UnsafeFileName { .. }
            ) {
                tracing::warn!("ignored transfer file-name event with unsafe path");
            }
            // Only the structured validation error is logged, never the event payload.
            tracing::debug!(
                session_id = %state.session_id,
                ?phase,
                event_kind,
                %error,
                "ignored transfer event that violates session lifecycle"
            );
            return false;
        }
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
