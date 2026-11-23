//! 事件顺序、会话和终态校验。

use super::payload::{
    TransferEventData, TransferEventEnvelope, contains_invalid_progress,
    contains_unsafe_event_file_name,
};
use super::types::{Role, TRANSFER_EVENT_SCHEMA_VERSION, TransferSessionId};
use std::fmt::Formatter;

/// Error returned when an event violates the ordering or lifecycle rules of one stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEventStreamError {
    /// The event uses a schema version that this consumer does not understand.
    UnsupportedSchemaVersion { found: u16 },
    /// The first event in a stream must use sequence `1`.
    FirstSequenceMustBeOne { found: u64 },
    /// The first event in a stream must carry the `started` payload.
    FirstEventMustBeStarted { sequence: u64 },
    /// All events in one stream must use the same session identifier.
    SessionChanged {
        expected: TransferSessionId,
        found: TransferSessionId,
    },
    /// All events in one stream must keep the role established by its first event.
    RoleChanged { expected: Role, found: Role },
    /// An accepted event must use the next contiguous sequence number.
    SequenceMismatch { expected: u64, found: u64 },
    /// A second `started` payload is not valid after the stream begins.
    StartedAfterStart { sequence: u64 },
    /// No event is accepted after a terminal payload.
    EventAfterTerminal { sequence: u64 },
    /// A file-name payload contains an absolute, traversing, or otherwise unsafe path.
    UnsafeFileName { sequence: u64 },
    /// A progress payload exceeds its declared total or contains a non-finite value.
    InvalidProgress { sequence: u64 },
    /// A non-terminal event cannot advance beyond the representable sequence range.
    SequenceExhausted { sequence: u64 },
}

impl std::fmt::Display for TransferEventStreamError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion { found } => {
                write!(
                    formatter,
                    "unsupported transfer event schema version {found}"
                )
            }
            Self::FirstSequenceMustBeOne { found } => {
                write!(
                    formatter,
                    "first transfer event must use sequence 1, found {found}"
                )
            }
            Self::FirstEventMustBeStarted { sequence } => {
                write!(
                    formatter,
                    "first transfer event at sequence {sequence} must be started"
                )
            }
            Self::SessionChanged { expected, found } => write!(
                formatter,
                "transfer event session changed from {expected} to {found}"
            ),
            Self::RoleChanged { expected, found } => write!(
                formatter,
                "transfer event role changed from {} to {}",
                expected.as_str(),
                found.as_str()
            ),
            Self::SequenceMismatch { expected, found } => write!(
                formatter,
                "expected transfer event sequence {expected}, found {found}"
            ),
            Self::StartedAfterStart { sequence } => {
                write!(formatter, "started event repeated at sequence {sequence}")
            }
            Self::EventAfterTerminal { sequence } => {
                write!(
                    formatter,
                    "transfer event arrived after terminal event at sequence {sequence}"
                )
            }
            Self::UnsafeFileName { sequence } => {
                write!(
                    formatter,
                    "transfer event at sequence {sequence} contains unsafe file name"
                )
            }
            Self::InvalidProgress { sequence } => {
                write!(
                    formatter,
                    "transfer event at sequence {sequence} contains invalid progress"
                )
            }
            Self::SequenceExhausted { sequence } => {
                write!(formatter, "transfer event sequence exhausted at {sequence}")
            }
        }
    }
}

impl std::error::Error for TransferEventStreamError {}

/// Stateful validator for one consumer-facing transfer event stream.
///
/// The validator accepts one `Started` event at sequence `1`, then only events from the same
/// session and role with contiguous sequence numbers. The first accepted event binds both the
/// session identifier and its role; a consumer must create a new validator for another stream.
/// Once an input violates a rule, the validator stays rejected and returns the original error
/// for later inputs, without exposing any event payload in its diagnostics.
#[derive(Debug, Clone)]
pub struct TransferEventStreamValidator {
    session_id: Option<TransferSessionId>,
    role: Option<Role>,
    expected_sequence: u64,
    terminal: bool,
    rejected: Option<TransferEventStreamError>,
}

impl Default for TransferEventStreamValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl TransferEventStreamValidator {
    /// Create an empty validator that expects a `Started` event with sequence `1`.
    pub const fn new() -> Self {
        Self {
            session_id: None,
            role: None,
            expected_sequence: 1,
            terminal: false,
            rejected: None,
        }
    }

    /// Validate and record one event, rejecting the stream permanently on the first violation.
    pub fn accept(
        &mut self,
        event: &TransferEventEnvelope,
    ) -> Result<(), TransferEventStreamError> {
        if let Some(error) = self.rejected.as_ref() {
            return Err(error.clone());
        }
        if event.schema_version != TRANSFER_EVENT_SCHEMA_VERSION {
            return self.reject(TransferEventStreamError::UnsupportedSchemaVersion {
                found: event.schema_version,
            });
        }

        if self.session_id.is_none() {
            if event.sequence != 1 {
                return self.reject(TransferEventStreamError::FirstSequenceMustBeOne {
                    found: event.sequence,
                });
            }
            if !matches!(event.event, TransferEventData::Started) {
                return self.reject(TransferEventStreamError::FirstEventMustBeStarted {
                    sequence: event.sequence,
                });
            }
            self.session_id = Some(event.session_id.clone());
            self.role = Some(event.role);
            self.expected_sequence = 2;
            return Ok(());
        }

        if self
            .session_id
            .as_ref()
            .is_some_and(|session_id| session_id != &event.session_id)
        {
            return self.reject(TransferEventStreamError::SessionChanged {
                expected: self.session_id.clone().expect("session ID is present"),
                found: event.session_id.clone(),
            });
        }
        if self.terminal {
            return self.reject(TransferEventStreamError::EventAfterTerminal {
                sequence: event.sequence,
            });
        }
        if event.sequence != self.expected_sequence {
            return self.reject(TransferEventStreamError::SequenceMismatch {
                expected: self.expected_sequence,
                found: event.sequence,
            });
        }
        if let Some(expected) = self.role
            && expected != event.role
        {
            return self.reject(TransferEventStreamError::RoleChanged {
                expected,
                found: event.role,
            });
        }
        if contains_unsafe_event_file_name(&event.event) {
            return self.reject(TransferEventStreamError::UnsafeFileName {
                sequence: event.sequence,
            });
        }
        if contains_invalid_progress(&event.event) {
            return self.reject(TransferEventStreamError::InvalidProgress {
                sequence: event.sequence,
            });
        }
        if matches!(event.event, TransferEventData::Started) {
            return self.reject(TransferEventStreamError::StartedAfterStart {
                sequence: event.sequence,
            });
        }
        if event.event.is_terminal() {
            self.terminal = true;
            return Ok(());
        }
        let Some(next_sequence) = event.sequence.checked_add(1) else {
            return self.reject(TransferEventStreamError::SequenceExhausted {
                sequence: event.sequence,
            });
        };
        self.expected_sequence = next_sequence;
        Ok(())
    }

    /// Return whether a terminal event has been accepted.
    pub const fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Return whether an invalid event has permanently rejected this stream.
    pub const fn is_rejected(&self) -> bool {
        self.rejected.is_some()
    }

    fn reject(&mut self, error: TransferEventStreamError) -> Result<(), TransferEventStreamError> {
        self.rejected = Some(error.clone());
        Err(error)
    }
}
