//! Core library: 负责实现发送/接收逻辑及进度/类型定义。
//!
//! Private implementation modules. Public library contracts are re-exported
//! explicitly by the crate root; frontends use the same public API.
pub mod events;
mod filesystem;
pub mod manifest;
pub mod options;
mod progress;
pub mod receive_cache;
pub mod receiver;
pub mod recovery;
pub mod results;
pub mod sender;
mod shutdown;
mod storage;
mod transport;
pub use transport::endpoint_secret;
pub mod types;
