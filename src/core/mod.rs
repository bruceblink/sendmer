//! Core library: 负责实现发送/接收逻辑及进度/类型定义。
//!
//! 该模块导出内部子模块：`send`, `receive`, `progress`, `types`，
//! 并提供给上层 crate 使用的库 API（见 `src/lib.rs` 的 pub re-export）。
pub mod cli_helper;
pub mod receiver;
pub mod sender;
pub mod types;
