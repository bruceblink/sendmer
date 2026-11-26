//! 接收端功能：根据票据连接远端并导出数据到本地目录。
//!
//! 主要导出 `download`，它负责建立连接、跟踪进度并将文件导出到目标目录。

mod cleanup;
mod context;
mod download;
mod entry;
mod errors;
mod limits;
mod policy;
mod prepare;
mod recovery;
mod retry;
mod shutdown;

pub use entry::receive;
pub use entry::receive_with_cancellation;
pub use entry::receive_with_events;
pub use entry::receive_with_events_and_cancellation;
pub use entry::receive_with_limits;
pub use entry::receive_with_limits_and_cancellation;

const RECEIVE_TEMP_DIR_PREFIX: &str = ".sendmer-recv-";

#[cfg(test)]
mod tests;
