#![warn(clippy::all)]
#![warn(clippy::nursery)]

//! sendmer: small CLI for sharing/downloading blob data
//!
//! 这个 crate 暴露了库级 API（`start_share` 和 `download`）给程序调用，
//! 同时也包含用于命令行工具的包装（`src/bin`）。
pub mod cli;
pub mod core;

pub use core::{
    receiver::receive,
    sender::send,
    types::{
        AddrInfoOptions, AppHandle, EventEmitter, ReceiveOptions, ReceiveResult, RelayModeOption,
        SendOptions, SendResult,
    },
};
