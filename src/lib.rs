#![warn(clippy::all)]
#![warn(clippy::nursery)]

//! Sendmer's public, crate-root API for sending and receiving files.
//!
//! Use the opaque SendHandle to own a share and close or cancel its resources.
//! A receiving client can configure retry policy and limits without importing
//! internal modules.
//!
//! ```no_run
//! use sendmer::{EntryType, ReceiveOptions, ReceiveRetryPolicy, SendOptions, send_handle};
//!
//! async fn share(path: std::path::PathBuf) -> anyhow::Result<()> {
//!     let handle = send_handle(path, SendOptions::default()).await?;
//!     let _: EntryType = handle.entry_type();
//!     let _ = handle.ticket();
//!     handle.close().await
//! }
//!
//! let options = ReceiveOptions {
//!     retry_policy: ReceiveRetryPolicy { download_retry_limit: 4, ..Default::default() },
//!     ..Default::default()
//! };
//! assert_eq!(options.retry_policy.download_retry_limit, 4);
//! ```
//!
//! The implementation module is private:
//!
//! ```compile_fail
//! use sendmer::core::sender;
//! ```
//!
//! Legacy SendResult metadata and lifecycle methods remain available, but
//! internal resource handles are not public:
//!
//! ```compile_fail
//! # #![allow(deprecated)]
//! fn access_internal_router(result: sendmer::SendResult) {
//!     let _ = result.router;
//! }
//! ```
mod core;

// Retain the published argument types for one compatibility cycle. The CLI's
// presentation and updater implementation are private to the binary.
mod cli_args;
pub use cli_args::{
    Args, CacheArgs, CacheCommands, CachePruneArgs, Commands, CommonArgs, Format, ReceiveArgs,
    SendArgs,
};

/// Transfer manifest model. Its wire format is unchanged.
pub use core::manifest;

#[allow(deprecated)]
pub use core::{
    endpoint_secret,
    events::{
        AppHandle, EventEmitter, LegacyTransferEvent, ParseTransferSessionIdError, Role,
        TRANSFER_EVENT_SCHEMA_VERSION, TransferError, TransferErrorCode, TransferEvent,
        TransferEventData, TransferEventEnvelope, TransferEventStreamError,
        TransferEventStreamValidator, TransferPhase, TransferSessionId, emit_event,
    },
    options::{
        AddrInfoOptions, ReceiveCacheOptions, ReceiveLimits, ReceiveOptions, ReceiveRetryPolicy,
        RelayModeOption, SendOptions, apply_options,
    },
    receive_cache::{ReceiveCachePruneReport, prune_receive_cache},
    receiver::{
        receive, receive_with_events, receive_with_events_and_cancellation,
        receive_with_limits_and_cancellation,
    },
    recovery::{RECOVERY_SNAPSHOT_SCHEMA_VERSION, RecoverySnapshot, RecoveryTerminalState},
    results::{ReceiveResult, SendHandle, SenderTransferStatus},
    sender::{send_handle, send_handle_with_events},
    types::EntryType,
};

/// Compatibility entry point retained for one release cycle.
#[deprecated(since = "0.12.0", note = "use send_handle or send_handle_with_events")]
pub use core::sender::send;

/// Compatibility entry point retained for callers that need an event consumer.
#[deprecated(
    since = "0.12.0",
    note = "use receive_with_limits_and_cancellation with no cancellation receiver"
)]
pub use core::receiver::receive_with_limits;

/// Compatibility entry point retained for one release cycle.
#[deprecated(since = "0.12.0", note = "use receive_with_events_and_cancellation")]
pub use core::receiver::receive_with_cancellation;

/// Compatibility result type retained for one release cycle.
#[deprecated(since = "0.12.0", note = "use SendHandle")]
pub use core::results::SendResult;
