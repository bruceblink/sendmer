//! 版本化事件载荷和事件信封。

use super::types::{
    Role, TRANSFER_EVENT_SCHEMA_VERSION, TransferError, TransferPhase, TransferSessionId,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _};

/// Versioned event payload nested inside [`TransferEventEnvelope`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransferEventData {
    Started,
    Progress {
        processed: u64,
        total: u64,
        speed_bytes_per_sec: f64,
    },
    FileNames {
        file_names: Vec<String>,
    },
    Completed,
    Failed {
        error: TransferError,
    },
    Cancelled,
}

impl TransferEventData {
    /// Return whether this payload permanently closes its transfer session.
    pub const fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed { .. } | Self::Cancelled
        )
    }
}

/// Return whether a file name is safe to expose as a relative logical event path.
///
/// Event consumers must never receive absolute paths or traversal components. Forward slashes
/// are the only supported separator so that the same JSONL stream has one meaning on every host.
pub fn is_safe_event_file_name(name: &str) -> bool {
    if name.is_empty() || name.contains('\0') || name.starts_with('/') || name.starts_with('\\') {
        return false;
    }

    // Reject drive-qualified names such as `C:/secret` and `C:secret` before they reach a
    // Windows consumer. A colon in a later component is not a path root by itself.
    let first_component = name.split('/').next().unwrap_or_default();
    let first_bytes = first_component.as_bytes();
    if first_bytes.len() >= 2 && first_bytes[0].is_ascii_alphabetic() && first_bytes[1] == b':' {
        return false;
    }

    name.split('/').all(|component| {
        !component.is_empty() && component != "." && component != ".." && !component.contains('\\')
    })
}

pub(super) fn contains_unsafe_event_file_name(event: &TransferEventData) -> bool {
    matches!(
        event,
        TransferEventData::FileNames { file_names }
            if file_names
                .iter()
                .any(|name| !is_safe_event_file_name(name))
    )
}

/// 检查进度事件的数值边界，避免无穷大、负速率和超过总量的偏移进入公共事件流。
pub(super) fn contains_invalid_progress(event: &TransferEventData) -> bool {
    matches!(
        event,
        TransferEventData::Progress {
            processed,
            total,
            speed_bytes_per_sec,
        } if processed > total || !speed_bytes_per_sec.is_finite() || *speed_bytes_per_sec < 0.0
    )
}

/// Stable versioned envelope for JSON Lines and external event consumers.
#[derive(Debug, Clone, PartialEq)]
pub struct TransferEventEnvelope {
    pub schema_version: u16,
    pub session_id: TransferSessionId,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub role: Role,
    pub phase: TransferPhase,
    pub event: TransferEventData,
}

#[derive(Serialize)]
struct TransferEventEnvelopeRef<'a> {
    schema_version: u16,
    session_id: &'a TransferSessionId,
    sequence: u64,
    timestamp_ms: u64,
    role: Role,
    phase: TransferPhase,
    event: &'a TransferEventData,
}

/// Wire fields used to validate the event schema before exposing an envelope to consumers.
///
/// Serde still ignores unknown optional fields, so newer producers can add non-required data
/// without breaking an older consumer; an unknown schema version is rejected explicitly.
#[derive(Debug, Deserialize)]
struct TransferEventEnvelopeFields {
    schema_version: u16,
    session_id: TransferSessionId,
    sequence: u64,
    timestamp_ms: u64,
    role: Role,
    phase: TransferPhase,
    event: TransferEventData,
}

impl Serialize for TransferEventEnvelope {
    /// Serialize only an envelope that obeys the current schema and privacy boundary.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if self.schema_version != TRANSFER_EVENT_SCHEMA_VERSION {
            return Err(S::Error::custom(format!(
                "unsupported transfer event schema version {}",
                self.schema_version
            )));
        }
        if contains_unsafe_event_file_name(&self.event) {
            return Err(S::Error::custom("transfer event contains unsafe file name"));
        }
        if contains_invalid_progress(&self.event) {
            return Err(S::Error::custom("transfer event contains invalid progress"));
        }

        TransferEventEnvelopeRef {
            schema_version: self.schema_version,
            session_id: &self.session_id,
            sequence: self.sequence,
            timestamp_ms: self.timestamp_ms,
            role: self.role,
            phase: self.phase,
            event: &self.event,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TransferEventEnvelope {
    /// Deserialize one event only when its required schema version is understood.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields = TransferEventEnvelopeFields::deserialize(deserializer)?;
        if fields.schema_version != TRANSFER_EVENT_SCHEMA_VERSION {
            return Err(D::Error::custom(format!(
                "unsupported transfer event schema version {}",
                fields.schema_version
            )));
        }
        let envelope = Self {
            schema_version: fields.schema_version,
            session_id: fields.session_id,
            sequence: fields.sequence,
            timestamp_ms: fields.timestamp_ms,
            role: fields.role,
            phase: fields.phase,
            event: fields.event,
        };
        if contains_unsafe_event_file_name(&envelope.event) {
            return Err(D::Error::custom("transfer event contains unsafe file name"));
        }
        if contains_invalid_progress(&envelope.event) {
            return Err(D::Error::custom("transfer event contains invalid progress"));
        }
        Ok(envelope)
    }
}

impl TransferEventEnvelope {
    /// Construct one event with explicit ordering and timestamp values.
    pub const fn new(
        session_id: TransferSessionId,
        sequence: u64,
        timestamp_ms: u64,
        role: Role,
        phase: TransferPhase,
        event: TransferEventData,
    ) -> Self {
        Self {
            schema_version: TRANSFER_EVENT_SCHEMA_VERSION,
            session_id,
            sequence,
            timestamp_ms,
            role,
            phase,
            event,
        }
    }
}
