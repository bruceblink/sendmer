//! 传输进度、事件发射和 sender 会话聚合。
//!
//! 本模块只把核心状态转换为事件和进度数据，不负责网络连接、文件导出或 CLI 输出。

mod emitter;
mod receiver;
mod sender;
mod tracker;

pub use emitter::TransferEventEmitter;
pub use receiver::ReceiverProgressReporter;
pub use sender::{SenderProgressReporter, SenderTransferStatus};
pub use tracker::{CompletionStatus, ProgressTracker, ProviderProgressTracker, TransferId};

#[cfg(test)]
mod tests;
