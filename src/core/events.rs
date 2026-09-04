//! 事件发射器接口和传输事件定义。
//!
//! 本文件定义：事件发射器 trait、传输事件枚举、角色枚举。

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _, ser::Error as _};
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::Arc;

/// Current JSON schema version for [`TransferEventEnvelope`].
pub const TRANSFER_EVENT_SCHEMA_VERSION: u16 = 1;

/// Random application-level identifier shared by every event in one transfer session.
///
/// The canonical representation is 32 lowercase hexadecimal characters. It is intentionally
/// independent from tickets, content hashes, connections, and provider request identifiers.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TransferSessionId(String);

impl TransferSessionId {
    /// Generate a new opaque 128-bit session identifier.
    pub fn new() -> Self {
        Self(format!("{:032x}", rand::random::<u128>()))
    }

    /// Return the stable lowercase hexadecimal representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TransferSessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl Display for TransferSessionId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Error returned when a transfer session ID is not in its canonical wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseTransferSessionIdError;

impl Display for ParseTransferSessionIdError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("transfer session ID must contain 32 lowercase hexadecimal characters")
    }
}

impl std::error::Error for ParseTransferSessionIdError {}

impl FromStr for TransferSessionId {
    type Err = ParseTransferSessionIdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let is_canonical = value.len() == 32
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if !is_canonical {
            return Err(ParseTransferSessionIdError);
        }
        Ok(Self(value.to_owned()))
    }
}

impl Serialize for TransferSessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TransferSessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

/// Stable application-level phase for a transfer event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferPhase {
    Preparing,
    Connecting,
    Metadata,
    Transferring,
    Exporting,
    Finalizing,
}

/// Stable error categories exposed to event consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferErrorCode {
    InvalidInput,
    ConnectionFailed,
    Timeout,
    RemoteRejected,
    TransferInterrupted,
    TargetConflict,
    Filesystem,
    Internal,
}

/// Safe error information carried by a failed terminal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferError {
    pub code: TransferErrorCode,
    pub phase: TransferPhase,
    pub retryable: bool,
    pub message: String,
}

impl TransferError {
    /// Build a structured error without exposing an internal error chain.
    pub fn new(
        code: TransferErrorCode,
        phase: TransferPhase,
        retryable: bool,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            phase,
            retryable,
            message: message.into(),
        }
    }
}

/// Internal wrapper that preserves the diagnostic error chain alongside safe event details.
#[derive(Debug)]
struct ClassifiedTransferError {
    details: TransferError,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl Display for ClassifiedTransferError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        Display::fmt(&self.source, formatter)
    }
}

impl std::error::Error for ClassifiedTransferError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Attach stable event details without discarding the original diagnostic error chain.
pub(crate) fn classify_transfer_error(
    error: anyhow::Error,
    details: TransferError,
) -> anyhow::Error {
    if error.downcast_ref::<ClassifiedTransferError>().is_some() {
        return error;
    }
    anyhow::Error::new(ClassifiedTransferError {
        details,
        source: error.into_boxed_dyn_error(),
    })
}

/// Read structured details previously attached at the failure site.
pub(crate) fn classified_transfer_error(error: &anyhow::Error) -> Option<TransferError> {
    error
        .downcast_ref::<ClassifiedTransferError>()
        .map(|classified| classified.details.clone())
}

/// Marker error used to distinguish explicit cancellation from ordinary failures.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TransferCancelled;

impl Display for TransferCancelled {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Operation cancelled")
    }
}

impl std::error::Error for TransferCancelled {}

pub(crate) fn transfer_cancelled_error() -> anyhow::Error {
    anyhow::Error::new(TransferCancelled)
}

pub(crate) fn is_transfer_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<TransferCancelled>().is_some()
}

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
pub(crate) fn is_safe_event_file_name(name: &str) -> bool {
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

fn contains_unsafe_event_file_name(event: &TransferEventData) -> bool {
    matches!(
        event,
        TransferEventData::FileNames { file_names }
            if file_names
                .iter()
                .any(|name| !is_safe_event_file_name(name))
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
    /// An accepted event must use the next contiguous sequence number.
    SequenceMismatch { expected: u64, found: u64 },
    /// A second `started` payload is not valid after the stream begins.
    StartedAfterStart { sequence: u64 },
    /// No event is accepted after a terminal payload.
    EventAfterTerminal { sequence: u64 },
    /// A file-name payload contains an absolute, traversing, or otherwise unsafe path.
    UnsafeFileName { sequence: u64 },
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
/// session with contiguous sequence numbers. Once an input violates a rule, the validator stays
/// rejected and returns the original error for later inputs.
#[derive(Debug, Clone)]
pub struct TransferEventStreamValidator {
    session_id: Option<TransferSessionId>,
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
        if contains_unsafe_event_file_name(&event.event) {
            return self.reject(TransferEventStreamError::UnsafeFileName {
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

/// Public event type emitted by sendmer v0.8 integrations.
pub type TransferEvent = TransferEventEnvelope;

/// 事件发射器接口。
///
/// 库代码通过该 trait 将 [`TransferEventEnvelope`]
/// 发送到 CLI / Tauri / GUI 等不同前端实现。
///
/// 设计约束：
/// - 不返回 `Result`
/// - 事件发送失败不得影响主流程
/// - 实现应尽量做到非阻塞
pub trait EventEmitter: Send + Sync {
    /// 发射一个传输事件。
    fn emit(&self, event: &TransferEventEnvelope);
}

/// 传输过程中对外发送的统一事件模型。
///
/// 该枚举用于描述一次传输在某个角色（发送端 / 接收端）下的
/// 生命周期状态变化。
///
/// ⚠️ 注意：
/// - 这是**通知型事件**，不参与错误控制流
/// - 不用于 `Result` / `anyhow`
/// - payload 直接体现在枚举字段中
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[deprecated(
    since = "0.8.0",
    note = "use TransferEventEnvelope and match TransferEventData instead"
)]
pub enum LegacyTransferEvent {
    /// 传输开始
    Started { role: Role },

    /// 传输进度更新
    Progress {
        role: Role,
        /// 已处理字节数
        processed: u64,
        /// 总字节数
        total: u64,
        /// 当前速度（字节 / 秒）
        speed: f64,
    },

    /// 传输完成
    Completed { role: Role },

    /// 传输失败
    Failed {
        role: Role,
        /// 用于展示的错误信息
        message: String,
    },

    /// 特殊事件：文件名列表
    FileNames { role: Role, file_names: Vec<String> },
}

#[allow(deprecated)]
impl LegacyTransferEvent {
    /// 返回事件状态字符串（started / progress / completed / failed）
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::Progress { .. } => "progress",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::FileNames { .. } => "file-names",
        }
    }

    /// 返回事件所属角色
    pub const fn role(&self) -> Role {
        match self {
            Self::Started { role }
            | Self::Completed { role }
            | Self::Failed { role, .. }
            | Self::Progress { role, .. }
            | Self::FileNames { role, .. } => *role,
        }
    }

    /// 返回发送给 Tauri 前端的最终事件名
    ///
    /// 事件格式：
    /// `transfer:<role>:<state>`
    ///
    /// 示例：
    /// - `transfer:sender:started`
    /// - `transfer:receiver:progress`
    pub fn event_name(&self) -> String {
        format!("transfer:{}:{}", self.role().as_str(), self.state())
    }
}

/// 传输事件所属的角色（发送端 / 接收端）。
///
/// 用于区分事件来自哪一侧，
/// 前端与 CLI 可以据此展示不同视角的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// 数据发送方
    Sender,
    /// 数据接收方
    Receiver,
}

impl Role {
    /// 用于事件字符串拼接（Tauri 前端）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }
}

/// 应用层句柄：可选包装的共享 `EventEmitter`。
///
/// 使用 `None` 表示不发射任何事件（例如在测试或禁止进度时）。
pub type AppHandle = Option<Arc<dyn EventEmitter>>;

/// 安全地向前端发送事件。
///
/// 若未配置事件发射器或发送失败，将被忽略。
pub fn emit_event(app: &AppHandle, event: &TransferEventEnvelope) {
    if let Some(handle) = app {
        handle.emit(event);
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::{
        LegacyTransferEvent, Role, TRANSFER_EVENT_SCHEMA_VERSION, TransferError, TransferErrorCode,
        TransferEventData, TransferEventEnvelope, TransferEventStreamError,
        TransferEventStreamValidator, TransferPhase, TransferSessionId, classified_transfer_error,
        classify_transfer_error, is_safe_event_file_name, is_transfer_cancelled,
        transfer_cancelled_error,
    };
    use std::str::FromStr;

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
        let fixture = include_str!("../../tests/fixtures/transfer_event_v1_progress.json");
        let fixture_value: serde_json::Value =
            serde_json::from_str(fixture).expect("parse event fixture");

        assert_eq!(
            serde_json::to_value(&event).expect("serialize event envelope"),
            fixture_value
        );
        assert_eq!(
            serde_json::from_str::<TransferEventEnvelope>(fixture)
                .expect("deserialize event fixture"),
            event
        );
        assert_eq!(event.schema_version, TRANSFER_EVENT_SCHEMA_VERSION);
    }

    #[test]
    fn versioned_event_rejects_unknown_schema_versions() {
        let fixture = include_str!("../../tests/fixtures/transfer_event_unknown_schema.json");
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
        let fixture = include_str!("../../tests/fixtures/transfer_event_v1_progress.json");
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

    fn stream_event(
        session_id: &str,
        sequence: u64,
        event: TransferEventData,
    ) -> TransferEventEnvelope {
        TransferEventEnvelope::new(
            TransferSessionId::from_str(session_id).expect("valid session ID"),
            sequence,
            1_786_982_400_000,
            Role::Receiver,
            TransferPhase::Transferring,
            event,
        )
    }

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
        let decoded: TransferSessionId =
            serde_json::from_str(&json).expect("deserialize session ID");

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
}
