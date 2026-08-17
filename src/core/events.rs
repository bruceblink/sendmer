//! 事件发射器接口和传输事件定义。
//!
//! 本文件定义：事件发射器 trait、传输事件枚举、角色枚举。

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
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

/// Stable versioned envelope for JSON Lines and external event consumers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferEventEnvelope {
    pub schema_version: u16,
    pub session_id: TransferSessionId,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub role: Role,
    pub phase: TransferPhase,
    pub event: TransferEventData,
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
mod tests {
    use super::{
        LegacyTransferEvent, Role, TRANSFER_EVENT_SCHEMA_VERSION, TransferError, TransferErrorCode,
        TransferEventData, TransferEventEnvelope, TransferPhase, TransferSessionId,
        classified_transfer_error, classify_transfer_error, is_transfer_cancelled,
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
