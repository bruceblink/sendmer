#![warn(clippy::all)]
#![warn(clippy::nursery)]

//! sendmer: small CLI for send/receive blob data
//!
//! 这个 crate 暴露了库级 API（`send` 和 `receive`）给程序调用，
//! 同时也包含用于命令行工具的包装（`src/bin`）。
pub mod core;
mod transfer;

pub use core::{
    events::{emit_event, AppHandle, EventEmitter, Role, TransferEvent},
    options::{apply_options, AddrInfoOptions, ReceiveOptions, RelayModeOption, SendOptions},
    receiver::receive,
    results::{ReceiveResult, SendResult},
    sender::send,
    types::{Args, Commands, ReceiveArgs, SendArgs},
};
