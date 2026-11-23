use crate::core::events::{
    Role, TRANSFER_EVENT_SCHEMA_VERSION, TransferError, TransferErrorCode, TransferEventData,
    TransferEventEnvelope, TransferEventStreamError, TransferEventStreamValidator, TransferPhase,
    TransferSessionId, classified_transfer_error, classify_transfer_error, is_transfer_cancelled,
    transfer_cancelled_error,
};
use std::str::FromStr;

use super::stream_event;

#[test]
fn event_stream_validator_accepts_contiguous_session() {
    let session = "0123456789abcdef0123456789abcdef";
    let mut validator = TransferEventStreamValidator::new();

    assert_eq!(
        validator.accept(&stream_event(session, 1, TransferEventData::Started)),
        Ok(())
    );
    assert_eq!(
        validator.accept(&stream_event(
            session,
            2,
            TransferEventData::Progress {
                processed: 1,
                total: 2,
                speed_bytes_per_sec: 1.0,
            },
        )),
        Ok(())
    );
    assert_eq!(
        validator.accept(&stream_event(session, 3, TransferEventData::Completed)),
        Ok(())
    );
    assert!(validator.is_terminal());
    assert!(!validator.is_rejected());
}

#[test]
fn event_stream_validator_rejects_duplicate_and_gap_sequences() {
    let session = "0123456789abcdef0123456789abcdef";
    let mut duplicate_validator = TransferEventStreamValidator::new();
    duplicate_validator
        .accept(&stream_event(session, 1, TransferEventData::Started))
        .expect("started event");
    duplicate_validator
        .accept(&stream_event(
            session,
            2,
            TransferEventData::Progress {
                processed: 1,
                total: 2,
                speed_bytes_per_sec: 1.0,
            },
        ))
        .expect("first progress event");
    let duplicate = duplicate_validator
        .accept(&stream_event(
            session,
            2,
            TransferEventData::Progress {
                processed: 2,
                total: 2,
                speed_bytes_per_sec: 1.0,
            },
        ))
        .expect_err("duplicate sequence");
    assert_eq!(
        duplicate,
        TransferEventStreamError::SequenceMismatch {
            expected: 3,
            found: 2,
        }
    );
    assert!(duplicate_validator.is_rejected());
    assert_eq!(
        duplicate_validator.accept(&stream_event(session, 3, TransferEventData::Completed)),
        Err(duplicate)
    );

    let mut gap_validator = TransferEventStreamValidator::new();
    gap_validator
        .accept(&stream_event(session, 1, TransferEventData::Started))
        .expect("started event");
    let gap = gap_validator
        .accept(&stream_event(session, 4, TransferEventData::Completed))
        .expect_err("sequence gap");
    assert_eq!(
        gap,
        TransferEventStreamError::SequenceMismatch {
            expected: 2,
            found: 4,
        }
    );
}

#[test]
fn event_stream_validator_rejects_session_changes_and_repeated_start() {
    let first_session = "0123456789abcdef0123456789abcdef";
    let second_session = "fedcba9876543210fedcba9876543210";
    let mut validator = TransferEventStreamValidator::new();
    validator
        .accept(&stream_event(first_session, 1, TransferEventData::Started))
        .expect("started event");

    assert_eq!(
        validator.accept(&stream_event(
            second_session,
            2,
            TransferEventData::Progress {
                processed: 1,
                total: 2,
                speed_bytes_per_sec: 1.0,
            },
        )),
        Err(TransferEventStreamError::SessionChanged {
            expected: TransferSessionId::from_str(first_session).expect("session ID"),
            found: TransferSessionId::from_str(second_session).expect("session ID"),
        })
    );

    let mut repeated_start = TransferEventStreamValidator::new();
    repeated_start
        .accept(&stream_event(first_session, 1, TransferEventData::Started))
        .expect("started event");
    assert_eq!(
        repeated_start.accept(&stream_event(first_session, 2, TransferEventData::Started)),
        Err(TransferEventStreamError::StartedAfterStart { sequence: 2 })
    );
}

#[test]
fn event_stream_validator_rejects_role_changes_for_every_payload() {
    let session = "0123456789abcdef0123456789abcdef";
    let payloads = [
        TransferEventData::Progress {
            processed: 1,
            total: 2,
            speed_bytes_per_sec: 1.0,
        },
        TransferEventData::FileNames {
            file_names: vec!["safe.txt".to_owned()],
        },
        TransferEventData::Completed,
        TransferEventData::Failed {
            error: TransferError::new(
                TransferErrorCode::Internal,
                TransferPhase::Transferring,
                false,
                "private payload must not appear in diagnostics",
            ),
        },
        TransferEventData::Cancelled,
    ];
    for (expected, found) in [
        (Role::Receiver, Role::Sender),
        (Role::Sender, Role::Receiver),
    ] {
        for payload in &payloads {
            let mut validator = TransferEventStreamValidator::new();
            let mut started = stream_event(session, 1, TransferEventData::Started);
            started.role = expected;
            validator.accept(&started).expect("started event");
            let mut changed = stream_event(session, 2, payload.clone());
            changed.role = found;

            let error = validator
                .accept(&changed)
                .expect_err("role change must fail");
            assert_eq!(
                error,
                TransferEventStreamError::RoleChanged { expected, found }
            );
            assert!(error.to_string().contains("role changed"));
            assert!(!error.to_string().contains("private payload"));
            assert!(validator.is_rejected());
            assert!(!validator.is_terminal());

            changed.role = expected;
            assert_eq!(validator.accept(&changed), Err(error));
        }
    }
}

#[test]
fn event_stream_validator_accepts_each_terminal_for_both_roles() {
    let session = "0123456789abcdef0123456789abcdef";
    let terminals = [
        TransferEventData::Completed,
        TransferEventData::Failed {
            error: TransferError::new(
                TransferErrorCode::Timeout,
                TransferPhase::Transferring,
                true,
                "transfer timed out",
            ),
        },
        TransferEventData::Cancelled,
    ];
    for role in [Role::Sender, Role::Receiver] {
        for terminal in &terminals {
            let mut validator = TransferEventStreamValidator::new();
            let mut started = stream_event(session, 1, TransferEventData::Started);
            started.role = role;
            let mut finished = stream_event(session, 2, terminal.clone());
            finished.role = role;

            validator.accept(&started).expect("started event");
            validator
                .accept(&finished)
                .expect("same-role terminal event");
            assert!(validator.is_terminal());
            assert!(!validator.is_rejected());
        }
    }
}

#[test]
fn event_stream_validator_rejects_role_change_jsonl_fixture() {
    let events: Vec<TransferEventEnvelope> =
        include_str!("../../../../tests/fixtures/transfer_event_role_change.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid envelope in JSONL fixture"))
            .collect();
    let mut validator = TransferEventStreamValidator::new();
    validator.accept(&events[0]).expect("started event");
    let error = validator
        .accept(&events[1])
        .expect_err("fixture role change must fail");
    assert!(
        error
            .to_string()
            .contains("role changed from receiver to sender")
    );
    assert_eq!(validator.accept(&events[2]), Err(error));
    assert!(!validator.is_terminal());
}

#[test]
fn event_stream_validator_rejects_invalid_first_event_and_terminal_tail() {
    let session = "0123456789abcdef0123456789abcdef";
    let mut invalid_first_sequence = TransferEventStreamValidator::new();
    assert_eq!(
        invalid_first_sequence.accept(&stream_event(session, 2, TransferEventData::Started)),
        Err(TransferEventStreamError::FirstSequenceMustBeOne { found: 2 })
    );

    let mut invalid_first_payload = TransferEventStreamValidator::new();
    assert_eq!(
        invalid_first_payload.accept(&stream_event(session, 1, TransferEventData::Completed)),
        Err(TransferEventStreamError::FirstEventMustBeStarted { sequence: 1 })
    );

    let mut terminal_validator = TransferEventStreamValidator::new();
    terminal_validator
        .accept(&stream_event(session, 1, TransferEventData::Started))
        .expect("started event");
    terminal_validator
        .accept(&stream_event(session, 2, TransferEventData::Completed))
        .expect("completed event");
    assert_eq!(
        terminal_validator.accept(&stream_event(
            session,
            3,
            TransferEventData::Progress {
                processed: 2,
                total: 2,
                speed_bytes_per_sec: 1.0,
            },
        )),
        Err(TransferEventStreamError::EventAfterTerminal { sequence: 3 })
    );
}

#[test]
fn event_stream_validator_rejects_unknown_schema_before_acceptance() {
    let session = "0123456789abcdef0123456789abcdef";
    let mut event = stream_event(session, 1, TransferEventData::Started);
    event.schema_version = TRANSFER_EVENT_SCHEMA_VERSION + 1;
    let mut validator = TransferEventStreamValidator::new();

    assert_eq!(
        validator.accept(&event),
        Err(TransferEventStreamError::UnsupportedSchemaVersion { found: 2 })
    );
    assert!(validator.is_rejected());
}

#[test]
fn event_stream_validator_rejects_unsafe_file_names_without_echoing_them() {
    let session = "0123456789abcdef0123456789abcdef";
    let mut validator = TransferEventStreamValidator::new();
    validator
        .accept(&stream_event(session, 1, TransferEventData::Started))
        .expect("started event");

    let error = validator
        .accept(&stream_event(
            session,
            2,
            TransferEventData::FileNames {
                file_names: vec!["/secret.txt".to_owned()],
            },
        ))
        .expect_err("unsafe file name should be rejected");
    assert_eq!(
        error,
        TransferEventStreamError::UnsafeFileName { sequence: 2 }
    );
    assert!(!error.to_string().contains("/secret.txt"));
    assert!(validator.is_rejected());
}

#[test]
fn event_stream_validator_rejects_progress_beyond_total() {
    let session = "0123456789abcdef0123456789abcdef";
    let mut validator = TransferEventStreamValidator::new();
    validator
        .accept(&stream_event(session, 1, TransferEventData::Started))
        .expect("started event");

    let error = validator
        .accept(&stream_event(
            session,
            2,
            TransferEventData::Progress {
                processed: 2,
                total: 1,
                speed_bytes_per_sec: 1.0,
            },
        ))
        .expect_err("progress beyond total should be rejected");
    assert_eq!(
        error,
        TransferEventStreamError::InvalidProgress { sequence: 2 }
    );
}

#[test]
fn transfer_session_id_rejects_noncanonical_values() {
    for invalid in [
        "0123456789abcdef",
        "0123456789ABCDEF0123456789ABCDEF",
        "0123456789abcdef0123456789abcdeg",
    ] {
        assert!(TransferSessionId::from_str(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn generated_session_id_round_trips_through_json() {
    let session_id = TransferSessionId::new();
    let json = serde_json::to_string(&session_id).expect("serialize session ID");
    let decoded: TransferSessionId = serde_json::from_str(&json).expect("deserialize session ID");

    assert_eq!(session_id.as_str().len(), 32);
    assert_eq!(decoded, session_id);
}

#[test]
fn only_completed_failed_and_cancelled_are_terminal() {
    let failure = TransferEventData::Failed {
        error: TransferError::new(
            TransferErrorCode::ConnectionFailed,
            TransferPhase::Connecting,
            true,
            "unable to connect to the sender",
        ),
    };

    assert!(TransferEventData::Completed.is_terminal());
    assert!(failure.is_terminal());
    assert!(TransferEventData::Cancelled.is_terminal());
    assert!(!TransferEventData::Started.is_terminal());
    assert!(!TransferEventData::FileNames { file_names: vec![] }.is_terminal());
}

#[test]
fn classified_error_preserves_diagnostics_and_stable_details() {
    let details = TransferError::new(
        TransferErrorCode::ConnectionFailed,
        TransferPhase::Connecting,
        true,
        "unable to connect to the sender",
    );
    let error = classify_transfer_error(
        anyhow::anyhow!("connection refused").context("dial peer"),
        details.clone(),
    )
    .context("receive failed");

    assert_eq!(error.to_string(), "receive failed");
    assert_eq!(classified_transfer_error(&error), Some(details));
    assert!(format!("{error:#}").contains("connection refused"));
}

#[test]
fn cancellation_uses_a_typed_marker() {
    let error = transfer_cancelled_error();
    assert!(is_transfer_cancelled(&error));
    assert_eq!(error.to_string(), "Operation cancelled");
}
