use std::sync::{Arc, Mutex as StdMutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::core::events::{
    AppHandle, Role, TransferError, TransferEventData, TransferEventEnvelope, TransferPhase,
    TransferSessionId, emit_event, is_safe_event_file_name,
};

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

    /// Emit relative logical names without exposing absolute or traversing paths.
    pub fn emit_file_names(&self, file_names: Vec<String>) {
        if file_names.iter().any(|name| !is_safe_event_file_name(name)) {
            tracing::warn!(
                file_count = file_names.len(),
                "ignored transfer file-name event with unsafe path"
            );
            return;
        }
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
        let event_kind = match &event {
            TransferEventData::Started => "started",
            TransferEventData::Progress { .. } => "progress",
            TransferEventData::FileNames { .. } => "file_names",
            TransferEventData::Completed => "completed",
            TransferEventData::Failed { .. } => "failed",
            TransferEventData::Cancelled => "cancelled",
        };
        let is_started = matches!(event, TransferEventData::Started);
        if state.terminal || (is_started && state.started) || (!is_started && !state.started) {
            tracing::debug!(
                session_id = %state.session_id,
                ?phase,
                event_kind,
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
