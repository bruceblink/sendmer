//! 发送端功能：将本地文件/目录导入 Blob 存储并通过 iroh 协议对外提供。
//!
//! 主要导出 `start_share`，它会导入数据、启动路由器并返回用于后续管理的 `SendResult`。

mod import;
mod lifecycle;
mod provider;
mod setup;
mod validation;

pub use lifecycle::send;
pub use lifecycle::send_handle;
pub use lifecycle::send_handle_with_events;

#[cfg(test)]
mod tests;
