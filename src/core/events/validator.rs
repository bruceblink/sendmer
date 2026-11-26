//! Sticky consumer-facing validation of one event stream.

use super::payload::TransferEventEnvelope;
use super::state::EventStreamState;
use super::stream_error::TransferEventStreamError;

/// Stateful validator for one consumer-facing transfer event stream.
///
/// The first accepted event binds its session identifier and role. Contiguous
/// events must obey the shared phase and lifecycle rules. Once an input violates
/// a rule, the validator stays rejected and returns the original error.
#[derive(Debug, Clone)]
pub struct TransferEventStreamValidator {
    state: EventStreamState,
    rejected: Option<TransferEventStreamError>,
}

impl Default for TransferEventStreamValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferEventStreamValidator {
    /// Create an empty validator that expects a Started event with sequence 1.
    pub const fn new() -> Self {
        Self {
            state: EventStreamState::new(),
            rejected: None,
        }
    }

    /// Validate and record one event, rejecting the stream permanently on the first violation.
    pub fn accept(
        &mut self,
        event: &TransferEventEnvelope,
    ) -> Result<(), TransferEventStreamError> {
        if let Some(error) = &self.rejected {
            return Err(error.clone());
        }
        if let Err(error) = self.state.accept(event) {
            self.rejected = Some(error.clone());
            return Err(error);
        }
        Ok(())
    }

    /// Return whether a terminal event has been accepted.
    pub const fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }

    /// Return whether an invalid event has permanently rejected this stream.
    pub const fn is_rejected(&self) -> bool {
        self.rejected.is_some()
    }
}
