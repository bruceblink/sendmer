#![warn(clippy::all)]
#![warn(clippy::nursery)]

//! sendmer: small CLI for send/receive blob data
//!
//! 这个 crate 暴露了库级 API（`send` 和 `receive`）给程序调用，
//! 同时也包含用于命令行工具的包装（`src/bin`）。
pub mod core;

#[allow(deprecated)]
pub use core::{
    args::{Args, Commands, ReceiveArgs, SendArgs},
    events::{
        AppHandle, EventEmitter, LegacyTransferEvent, Role, TRANSFER_EVENT_SCHEMA_VERSION,
        TransferError, TransferErrorCode, TransferEvent, TransferEventData, TransferEventEnvelope,
        TransferEventStreamError, TransferEventStreamValidator, TransferPhase, TransferSessionId,
        emit_event,
    },
    options::{
        AddrInfoOptions, ReceiveCacheOptions, ReceiveLimits, ReceiveOptions, RelayModeOption,
        SendOptions, apply_options,
    },
    receive_cache::{ReceiveCachePruneReport, prune_receive_cache},
    receiver::{receive, receive_with_events, receive_with_events_and_cancellation},
    recovery::{RECOVERY_SNAPSHOT_SCHEMA_VERSION, RecoverySnapshot, RecoveryTerminalState},
    results::{ReceiveResult, SendHandle, SenderTransferStatus},
    sender::{send_handle, send_handle_with_events},
};

/// Compatibility entry point retained for one release cycle.
#[deprecated(since = "0.12.0", note = "use send_handle or send_handle_with_events")]
pub use core::sender::send;

/// Compatibility entry point retained for callers that need an event consumer.
#[deprecated(since = "0.12.0", note = "use receive or receive_with_events")]
pub use core::receiver::receive_with_limits;

/// Compatibility entry point retained for one release cycle.
#[deprecated(
    since = "0.12.0",
    note = "use receive or receive_with_limits_and_cancellation"
)]
pub use core::receiver::receive_with_cancellation;

/// Compatibility entry point retained for one release cycle.
#[deprecated(
    since = "0.12.0",
    note = "use receive or receive_with_limits_and_cancellation"
)]
pub use core::receiver::receive_with_limits_and_cancellation;

/// Compatibility result type retained for one release cycle.
#[deprecated(since = "0.12.0", note = "use SendHandle")]
pub use core::results::SendResult;
