use super::super::{
    EventStreamState, Role, TransferError, TransferErrorCode, TransferEventData,
    TransferEventEnvelope, TransferEventStreamError as Error, TransferEventStreamValidator,
    TransferPhase, TransferSessionId,
};
use TransferPhase::{Connecting, Exporting, Finalizing, Metadata, Preparing, Transferring};

fn envelope(
    role: Role,
    sequence: u64,
    phase: TransferPhase,
    data: TransferEventData,
) -> TransferEventEnvelope {
    TransferEventEnvelope::new(
        "0123456789abcdef0123456789abcdef"
            .parse::<TransferSessionId>()
            .unwrap(),
        sequence,
        0,
        role,
        phase,
        data,
    )
}

fn progress() -> TransferEventData {
    TransferEventData::Progress {
        processed: 1,
        total: 2,
        speed_bytes_per_sec: 1.0,
    }
}

fn names() -> TransferEventData {
    TransferEventData::FileNames {
        file_names: vec!["safe.txt".into()],
    }
}

fn failure(phase: TransferPhase) -> TransferEventData {
    TransferEventData::Failed {
        error: TransferError::new(
            TransferErrorCode::ConnectionFailed,
            phase,
            true,
            "private failure detail",
        ),
    }
}

fn active(role: Role, phase: TransferPhase) -> TransferEventStreamValidator {
    let mut validator = TransferEventStreamValidator::new();
    validator
        .accept(&envelope(role, 1, phase, TransferEventData::Started))
        .unwrap();
    validator
}

#[test]
fn started_phase_rules_are_explicit_for_both_roles() {
    for role in [Role::Sender, Role::Receiver] {
        for phase in [
            Preparing,
            Connecting,
            Metadata,
            Transferring,
            Exporting,
            Finalizing,
        ] {
            let mut validator = TransferEventStreamValidator::new();
            let result = validator.accept(&envelope(role, 1, phase, TransferEventData::Started));
            if matches!(phase, Exporting | Finalizing) {
                assert_eq!(
                    result,
                    Err(Error::InvalidPhase {
                        sequence: 1,
                        role,
                        previous: None,
                        found: phase,
                    })
                );
                assert!(validator.is_rejected());
            } else {
                assert_eq!(result, Ok(()));
                assert!(!validator.is_terminal());
            }
        }
    }
}

#[test]
fn validates_payload_phase_matrix_for_every_active_phase_and_role() {
    for role in [Role::Sender, Role::Receiver] {
        for previous in [Preparing, Connecting, Metadata, Transferring] {
            for phase in [
                Preparing,
                Connecting,
                Metadata,
                Transferring,
                Exporting,
                Finalizing,
            ] {
                for (payload, allowed) in [
                    (progress(), phase == Transferring),
                    (names(), phase == Metadata),
                    (TransferEventData::Completed, phase == Finalizing),
                    (TransferEventData::Cancelled, phase == previous),
                    (failure(phase), role == Role::Receiver || phase != Exporting),
                ] {
                    let terminal = payload.is_terminal();
                    let mut validator = active(role, previous);
                    let event = envelope(role, 2, phase, payload);
                    let result = validator.accept(&event);
                    if allowed {
                        assert_eq!(result, Ok(()), "{role:?} {previous:?} {event:?}");
                        assert_eq!(validator.is_terminal(), terminal);
                    } else {
                        let error = Error::InvalidPhase {
                            sequence: 2,
                            role,
                            previous: Some(previous),
                            found: phase,
                        };
                        assert_eq!(result, Err(error.clone()));
                        assert!(!validator.is_terminal());
                        assert!(validator.is_rejected());
                        assert!(!error.to_string().contains("private failure detail"));
                        // A valid later input cannot un-reject a consumer.
                        assert_eq!(
                            validator.accept(&envelope(
                                role,
                                2,
                                Finalizing,
                                TransferEventData::Completed
                            )),
                            Err(error)
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn receiver_names_after_progress_and_sender_multiple_requests_remain_valid() {
    for role in [Role::Receiver, Role::Sender] {
        let mut validator = active(role, Preparing);
        for (sequence, phase, data) in [
            (2, Transferring, progress()),
            (3, Transferring, progress()),
            (4, Metadata, names()),
            (5, Transferring, progress()),
            (6, Finalizing, TransferEventData::Completed),
        ] {
            validator
                .accept(&envelope(role, sequence, phase, data))
                .unwrap();
        }
        assert!(validator.is_terminal());
    }
}

#[test]
fn retry_failure_can_report_an_earlier_operation_phase() {
    let role = Role::Receiver;
    let mut validator = active(role, Preparing);
    validator
        .accept(&envelope(role, 2, Transferring, progress()))
        .unwrap();
    validator
        .accept(&envelope(role, 3, Connecting, failure(Connecting)))
        .unwrap();
    assert!(validator.is_terminal());
}

#[test]
fn failure_phase_must_match_envelope_and_sender_cannot_export() {
    for (role, phase, inner) in [
        (Role::Receiver, Transferring, Connecting),
        (Role::Sender, Exporting, Exporting),
    ] {
        let mut validator = active(role, Transferring);
        assert_eq!(
            validator.accept(&envelope(role, 2, phase, failure(inner))),
            Err(Error::InvalidPhase {
                sequence: 2,
                role,
                previous: Some(Transferring),
                found: phase,
            })
        );
        assert!(validator.is_rejected());
        assert!(!validator.is_terminal());
    }
}

#[test]
fn every_terminal_rejects_every_later_payload() {
    for role in [Role::Sender, Role::Receiver] {
        for (phase, terminal) in [
            (Finalizing, TransferEventData::Completed),
            (Connecting, failure(Connecting)),
            (Preparing, TransferEventData::Cancelled),
        ] {
            let mut validator = active(role, Preparing);
            validator
                .accept(&envelope(role, 2, phase, terminal))
                .unwrap();
            for (tail_phase, tail) in [
                (Preparing, TransferEventData::Started),
                (Metadata, names()),
                (Transferring, progress()),
                (Finalizing, TransferEventData::Completed),
                (Connecting, failure(Connecting)),
                (Preparing, TransferEventData::Cancelled),
            ] {
                let mut copy = validator.clone();
                assert_eq!(
                    copy.accept(&envelope(role, 3, tail_phase, tail)),
                    Err(Error::EventAfterTerminal { sequence: 3 })
                );
                assert!(copy.is_terminal());
                assert!(copy.is_rejected());
            }
        }
    }
}

#[test]
fn producer_rejection_leaves_sequence_and_phase_unchanged() {
    let role = Role::Sender;
    let mut state = EventStreamState::new();
    assert!(
        state
            .accept(&envelope(role, 1, Exporting, TransferEventData::Started))
            .is_err()
    );
    assert_eq!(state.next_sequence(), 1);
    state
        .accept(&envelope(role, 1, Preparing, TransferEventData::Started))
        .unwrap();
    assert!(
        state
            .accept(&envelope(role, 2, Connecting, progress()))
            .is_err()
    );
    assert_eq!(state.next_sequence(), 2);
    assert_eq!(state.current_phase(), Preparing);
    assert!(!state.is_terminal());
    state
        .accept(&envelope(role, 2, Preparing, TransferEventData::Cancelled))
        .unwrap();
    assert!(state.is_terminal());
}

#[test]
fn invalid_phase_jsonl_is_wire_compatible_but_rejected_as_a_stream() {
    let mut validator = TransferEventStreamValidator::new();
    let events: Vec<TransferEventEnvelope> =
        include_str!("../../../../tests/fixtures/transfer_event_invalid_phase.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    validator.accept(&events[0]).unwrap();
    let error = Error::InvalidPhase {
        sequence: 2,
        role: Role::Receiver,
        previous: Some(Preparing),
        found: Connecting,
    };
    assert_eq!(validator.accept(&events[1]), Err(error.clone()));
    assert_eq!(validator.accept(&events[2]), Err(error));
}
