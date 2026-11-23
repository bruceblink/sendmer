use crate::core::events::{
    LegacyTransferEvent, Role, TRANSFER_EVENT_SCHEMA_VERSION, TransferEventData,
    TransferEventEnvelope, TransferPhase, TransferSessionId, is_safe_event_file_name,
};
use std::str::FromStr;

use super::stream_event;

#[test]
fn transfer_event_json_schema_is_stable() {
    let event = LegacyTransferEvent::Progress {
        role: Role::Receiver,
        processed: 512,
        total: 1024,
        speed: 256.0,
    };

    let json = serde_json::to_string(&event).expect("serialize transfer event");
    assert_eq!(
        json,
        r#"{"type":"progress","role":"receiver","processed":512,"total":1024,"speed":256.0}"#
    );
    assert_eq!(
        serde_json::from_str::<LegacyTransferEvent>(&json).expect("deserialize transfer event"),
        event
    );
}

#[test]
fn file_names_use_snake_case_event_type() {
    let event = LegacyTransferEvent::FileNames {
        role: Role::Sender,
        file_names: vec!["one.txt".to_owned()],
    };

    let value = serde_json::to_value(event).expect("serialize file names event");
    assert_eq!(value["type"], "file_names");
    assert_eq!(value["role"], "sender");
}

#[test]
fn versioned_progress_event_matches_json_fixture() {
    let event = TransferEventEnvelope::new(
        TransferSessionId::from_str("0123456789abcdef0123456789abcdef")
            .expect("valid fixture session ID"),
        3,
        1_786_982_400_000,
        Role::Receiver,
        TransferPhase::Transferring,
        TransferEventData::Progress {
            processed: 524_288,
            total: 1_048_576,
            speed_bytes_per_sec: 262_144.0,
        },
    );
    let fixture = include_str!("../../../../tests/fixtures/transfer_event_v1_progress.json");
    let fixture_value: serde_json::Value =
        serde_json::from_str(fixture).expect("parse event fixture");

    assert_eq!(
        serde_json::to_value(&event).expect("serialize event envelope"),
        fixture_value
    );
    assert_eq!(
        serde_json::from_str::<TransferEventEnvelope>(fixture).expect("deserialize event fixture"),
        event
    );
    assert_eq!(event.schema_version, TRANSFER_EVENT_SCHEMA_VERSION);
}

#[test]
fn versioned_event_rejects_unknown_schema_versions() {
    let fixture = include_str!("../../../../tests/fixtures/transfer_event_unknown_schema.json");
    let fixture: serde_json::Value = serde_json::from_str(fixture).expect("parse fixture");
    for version in [0, 2, u16::MAX] {
        let mut value = fixture.clone();
        value["schema_version"] = serde_json::Value::from(version);
        let error = serde_json::from_value::<TransferEventEnvelope>(value)
            .expect_err("unknown schema version must be rejected");
        assert!(
            error
                .to_string()
                .contains("unsupported transfer event schema version"),
            "unexpected error for schema {version}: {error}"
        );
    }
}

#[test]
fn versioned_event_ignores_unknown_optional_fields() {
    let fixture = include_str!("../../../../tests/fixtures/transfer_event_v1_progress.json");
    let mut value: serde_json::Value = serde_json::from_str(fixture).expect("parse fixture");
    value["future_optional_field"] = serde_json::json!({"kept": true});
    value["event"]["future_optional_field"] = serde_json::Value::from("ignored");

    let event = serde_json::from_value::<TransferEventEnvelope>(value)
        .expect("unknown optional fields should remain compatible");
    assert_eq!(event.schema_version, TRANSFER_EVENT_SCHEMA_VERSION);
    assert!(matches!(event.event, TransferEventData::Progress { .. }));
}

#[test]
fn event_file_names_accept_only_relative_logical_paths() {
    for valid in [
        "file.txt",
        "dir/sub/file.txt",
        "中文/报告.txt",
        "name:with-colon",
    ] {
        assert!(
            is_safe_event_file_name(valid),
            "expected safe name: {valid}"
        );
    }
    for invalid in [
        "",
        "/etc/passwd",
        "\\windows\\system32",
        "../secret.txt",
        "dir/../../secret.txt",
        "dir\\file.txt",
        "foo//bar",
        "./file.txt",
        "C:/secret.txt",
        "C:secret.txt",
        "file\0name.txt",
    ] {
        assert!(
            !is_safe_event_file_name(invalid),
            "expected unsafe name: {invalid:?}"
        );
    }
}

#[test]
fn versioned_event_serialization_rejects_unknown_schema_and_unsafe_names() {
    let session = "0123456789abcdef0123456789abcdef";
    let mut unknown_schema = stream_event(session, 1, TransferEventData::Started);
    unknown_schema.schema_version = TRANSFER_EVENT_SCHEMA_VERSION + 1;
    let schema_error = serde_json::to_string(&unknown_schema)
        .expect_err("unknown schema should not be serialized");
    assert!(
        schema_error
            .to_string()
            .contains("unsupported transfer event schema version")
    );

    let unsafe_name = "/secret.txt";
    let unsafe_event = stream_event(
        session,
        2,
        TransferEventData::FileNames {
            file_names: vec![unsafe_name.to_owned()],
        },
    );
    let name_error = serde_json::to_string(&unsafe_event)
        .expect_err("unsafe file name should not be serialized");
    assert!(name_error.to_string().contains("unsafe file name"));
    assert!(!name_error.to_string().contains(unsafe_name));
}

#[test]
fn versioned_event_deserialization_rejects_unsafe_names() {
    let value = serde_json::json!({
        "schema_version": TRANSFER_EVENT_SCHEMA_VERSION,
        "session_id": "0123456789abcdef0123456789abcdef",
        "sequence": 2,
        "timestamp_ms": 1_786_982_400_000u64,
        "role": "receiver",
        "phase": "metadata",
        "event": {"type": "file_names", "file_names": ["../secret.txt"]}
    });
    let error = serde_json::from_value::<TransferEventEnvelope>(value)
        .expect_err("unsafe file name should not be deserialized");
    assert!(error.to_string().contains("unsafe file name"));
    assert!(!error.to_string().contains("../secret.txt"));
}

#[test]
fn versioned_progress_rejects_non_finite_and_overflow_values() {
    let session = "0123456789abcdef0123456789abcdef";
    for event in [
        stream_event(
            session,
            2,
            TransferEventData::Progress {
                processed: 2,
                total: 1,
                speed_bytes_per_sec: 1.0,
            },
        ),
        stream_event(
            session,
            2,
            TransferEventData::Progress {
                processed: 1,
                total: 1,
                speed_bytes_per_sec: f64::NAN,
            },
        ),
    ] {
        let error =
            serde_json::to_string(&event).expect_err("invalid progress must not be serialized");
        assert!(error.to_string().contains("invalid progress"));
    }
}
