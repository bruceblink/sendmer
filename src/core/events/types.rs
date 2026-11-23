//! 传输事件共享类型和结构化错误。

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};
use std::fmt::{Display, Formatter};
use std::str::FromStr;

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
pub fn classify_transfer_error(error: anyhow::Error, details: TransferError) -> anyhow::Error {
    if error.downcast_ref::<ClassifiedTransferError>().is_some() {
        return error;
    }
    anyhow::Error::new(ClassifiedTransferError {
        details,
        source: error.into_boxed_dyn_error(),
    })
}

/// Read structured details previously attached at the failure site.
pub fn classified_transfer_error(error: &anyhow::Error) -> Option<TransferError> {
    error
        .downcast_ref::<ClassifiedTransferError>()
        .map(|classified| classified.details.clone())
}

/// Marker error used to distinguish explicit cancellation from ordinary failures.
#[derive(Debug, Clone, Copy)]
pub struct TransferCancelled;

impl Display for TransferCancelled {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Operation cancelled")
    }
}

impl std::error::Error for TransferCancelled {}

pub fn transfer_cancelled_error() -> anyhow::Error {
    anyhow::Error::new(TransferCancelled)
}

pub fn is_transfer_cancelled(error: &anyhow::Error) -> bool {
    error.downcast_ref::<TransferCancelled>().is_some()
}

/// 传输事件所属的角色。一个事件会话在首条事件后固定角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Sender,
    Receiver,
}

impl Role {
    /// Return the stable wire value used by legacy consumers.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }
}
