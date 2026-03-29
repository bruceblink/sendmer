#![warn(clippy::all)]
#![warn(clippy::nursery)]

//! sendmer: small CLI for send/receive blob data
//!
//! 这个 crate 暴露了库级 API（`send` 和 `receive`）给程序调用，
//! 同时也包含用于命令行工具的包装（`src/bin`）。
pub mod core;

pub use core::{
    args::{Args, Commands, ReceiveArgs, SendArgs},
    events::{AppHandle, EventEmitter, Role, TransferEvent, emit_event},
    options::{AddrInfoOptions, ReceiveOptions, RelayModeOption, SendOptions, apply_options},
    receiver::receive,
    results::{ReceiveResult, SendResult},
    sender::send,
};
