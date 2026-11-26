//! Shared internal state for producers and consumers of observable transfer events.
//!
//! This does not schedule transfers or model individual provider requests. A
//! session can omit phases and a failure can report the phase of an earlier retry.

use super::payload::{
    TransferEventData, TransferEventEnvelope, contains_invalid_progress,
    contains_unsafe_event_file_name,
};
use super::stream_error::TransferEventStreamError as Error;
use super::types::{Role, TRANSFER_EVENT_SCHEMA_VERSION, TransferPhase, TransferSessionId};

#[derive(Debug, Clone, Copy)]
enum Lifecycle {
    AwaitingStart,
    Active { role: Role, phase: TransferPhase },
    Completed(TransferPhase),
    Failed(TransferPhase),
    Cancelled(TransferPhase),
}

impl Lifecycle {
    const fn phase(self) -> Option<TransferPhase> {
        match self {
            Self::AwaitingStart => None,
            Self::Active { phase, .. }
            | Self::Completed(phase)
            | Self::Failed(phase)
            | Self::Cancelled(phase) => Some(phase),
        }
    }

    const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed(_) | Self::Failed(_) | Self::Cancelled(_)
        )
    }

    fn transition(self, event: &TransferEventEnvelope) -> Result<Self, Error> {
        let invalid_phase = || Error::InvalidPhase {
            sequence: event.sequence,
            role: event.role,
            previous: self.phase(),
            found: event.phase,
        };
        if matches!(self, Self::AwaitingStart) {
            if !matches!(event.event, TransferEventData::Started) {
                return Err(Error::FirstEventMustBeStarted {
                    sequence: event.sequence,
                });
            }
            // Normal producers begin at Preparing. Earlier API consumers also
            // start sessions at Connecting, Metadata or Transferring.
            if matches!(
                event.phase,
                TransferPhase::Exporting | TransferPhase::Finalizing
            ) {
                return Err(invalid_phase());
            }
            return Ok(Self::Active {
                role: event.role,
                phase: event.phase,
            });
        }

        let Self::Active { role, phase } = self else {
            return Err(Error::EventAfterTerminal {
                sequence: event.sequence,
            });
        };
        if role != event.role {
            return Err(Error::RoleChanged {
                expected: role,
                found: event.role,
            });
        }
        if contains_unsafe_event_file_name(&event.event) {
            return Err(Error::UnsafeFileName {
                sequence: event.sequence,
            });
        }
        if contains_invalid_progress(&event.event) {
            return Err(Error::InvalidProgress {
                sequence: event.sequence,
            });
        }
        match &event.event {
            TransferEventData::Started => Err(Error::StartedAfterStart {
                sequence: event.sequence,
            }),
            TransferEventData::Progress { .. } if event.phase == TransferPhase::Transferring => {
                Ok(Self::Active {
                    role,
                    phase: event.phase,
                })
            }
            // Names may become available only after the data/manifest is downloaded.
            // Preserve the v1 metadata-after-progress transition.
            TransferEventData::FileNames { .. } if event.phase == TransferPhase::Metadata => {
                Ok(Self::Active {
                    role,
                    phase: event.phase,
                })
            }
            TransferEventData::Completed if event.phase == TransferPhase::Finalizing => {
                Ok(Self::Completed(event.phase))
            }
            // Failure phase describes the failed operation, not a new active stage.
            // Reconnection can fail at Connecting after Transferring progress.
            TransferEventData::Failed { error }
                if error.phase == event.phase
                    && !(role == Role::Sender && event.phase == TransferPhase::Exporting) =>
            {
                Ok(Self::Failed(event.phase))
            }
            TransferEventData::Cancelled if event.phase == phase => {
                Ok(Self::Cancelled(event.phase))
            }
            _ => Err(invalid_phase()),
        }
    }
}

/// Mutable state shared by the emitter and validator. Rejections leave the state
/// unchanged; the public validator separately remembers the first rejection.
#[derive(Debug, Clone)]
pub struct EventStreamState {
    session_id: Option<TransferSessionId>,
    next_sequence: u64,
    lifecycle: Lifecycle,
}

impl EventStreamState {
    pub(crate) const fn new() -> Self {
        Self {
            session_id: None,
            next_sequence: 1,
            lifecycle: Lifecycle::AwaitingStart,
        }
    }

    pub(crate) const fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    pub(crate) const fn current_phase(&self) -> TransferPhase {
        match self.lifecycle.phase() {
            Some(phase) => phase,
            None => TransferPhase::Preparing,
        }
    }

    pub(crate) const fn is_terminal(&self) -> bool {
        self.lifecycle.is_terminal()
    }

    pub(crate) fn accept(&mut self, event: &TransferEventEnvelope) -> Result<(), Error> {
        if event.schema_version != TRANSFER_EVENT_SCHEMA_VERSION {
            return Err(Error::UnsupportedSchemaVersion {
                found: event.schema_version,
            });
        }
        if let Some(expected) = &self.session_id {
            if expected != &event.session_id {
                return Err(Error::SessionChanged {
                    expected: expected.clone(),
                    found: event.session_id.clone(),
                });
            }
            if self.is_terminal() {
                return Err(Error::EventAfterTerminal {
                    sequence: event.sequence,
                });
            }
            if event.sequence != self.next_sequence {
                return Err(Error::SequenceMismatch {
                    expected: self.next_sequence,
                    found: event.sequence,
                });
            }
        } else if event.sequence != 1 {
            return Err(Error::FirstSequenceMustBeOne {
                found: event.sequence,
            });
        }

        let lifecycle = self.lifecycle.transition(event)?;
        let next_sequence = if lifecycle.is_terminal() {
            event.sequence
        } else {
            event
                .sequence
                .checked_add(1)
                .ok_or(Error::SequenceExhausted {
                    sequence: event.sequence,
                })?
        };
        if self.session_id.is_none() {
            self.session_id = Some(event.session_id.clone());
        }
        self.lifecycle = lifecycle;
        self.next_sequence = next_sequence;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_exhaustion_rejects_nonterminal_but_allows_terminal() {
        let mut state = EventStreamState::new();
        let id = TransferSessionId::new();
        let start = TransferEventEnvelope::new(
            id.clone(),
            1,
            0,
            Role::Sender,
            TransferPhase::Preparing,
            TransferEventData::Started,
        );
        state.accept(&start).unwrap();
        state.next_sequence = u64::MAX;
        let progress = TransferEventEnvelope::new(
            id.clone(),
            u64::MAX,
            0,
            Role::Sender,
            TransferPhase::Transferring,
            TransferEventData::Progress {
                processed: 1,
                total: 2,
                speed_bytes_per_sec: 1.0,
            },
        );
        assert_eq!(
            state.accept(&progress),
            Err(Error::SequenceExhausted { sequence: u64::MAX })
        );
        assert_eq!(state.current_phase(), TransferPhase::Preparing);
        assert_eq!(state.next_sequence(), u64::MAX);
        assert!(!state.is_terminal());

        let terminal = TransferEventEnvelope::new(
            id,
            u64::MAX,
            0,
            Role::Sender,
            TransferPhase::Finalizing,
            TransferEventData::Completed,
        );
        state.accept(&terminal).unwrap();
        assert!(state.is_terminal());
        assert_eq!(
            state.accept(&terminal),
            Err(Error::EventAfterTerminal { sequence: u64::MAX })
        );
    }
}
