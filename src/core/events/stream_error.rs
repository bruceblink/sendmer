//! Stable consumer-facing event stream errors.

use super::types::{Role, TransferPhase, TransferSessionId};
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
    /// The event phase is incompatible with its role, payload or previous phase.
    InvalidPhase {
        sequence: u64,
        role: Role,
        previous: Option<TransferPhase>,
        found: TransferPhase,
    },
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
            Self::InvalidPhase {
                sequence,
                role,
                previous,
                found,
            } => write!(
                formatter,
                "invalid transfer phase at sequence {sequence}: role={}, previous={previous:?}, found={found:?}",
                role.as_str()
            ),
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
