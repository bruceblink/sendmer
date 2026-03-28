//! 事件发射器接口和传输事件定义。
//!
//! 本文件定义：事件发射器 trait、传输事件枚举、角色枚举。

use std::sync::Arc;

/// 事件发射器接口。
///
/// 库代码通过该 trait 将 [`TransferEvent`]
/// 发送到 CLI / Tauri / GUI 等不同前端实现。
///
/// 设计约束：
/// - 不返回 `Result`
/// - 事件发送失败不得影响主流程
/// - 实现应尽量做到非阻塞
pub trait EventEmitter: Send + Sync {
    /// 发射一个传输事件。
    fn emit(&self, event: &TransferEvent);
}

/// 传输过程中对外发送的统一事件模型。
///
/// 该枚举用于描述一次传输在某个角色（发送端 / 接收端）下的
/// 生命周期状态变化。
///
/// ⚠️ 注意：
/// - 这是**通知型事件**，不参与错误控制流
/// - 不用于 `Result` / `anyhow`
/// - payload 直接体现在枚举字段中
#[derive(Debug, Clone)]
pub enum TransferEvent {
    /// 传输开始
    Started { role: Role },

    /// 传输进度更新
    Progress {
        role: Role,
        /// 已处理字节数
        processed: u64,
        /// 总字节数
        total: u64,
        /// 当前速度（字节 / 秒）
        speed: f64,
    },

    /// 传输完成
    Completed { role: Role },

    /// 传输失败
    Failed {
        role: Role,
        /// 用于展示的错误信息
        message: String,
    },

    /// 特殊事件：文件名列表
    FileNames { role: Role, file_names: Vec<String> },
}

impl TransferEvent {
    /// 返回事件状态字符串（started / progress / completed / failed）
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::Progress { .. } => "progress",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::FileNames { .. } => "file-names",
        }
    }

    /// 返回事件所属角色
    pub const fn role(&self) -> Role {
        match self {
            Self::Started { role }
            | Self::Completed { role }
            | Self::Failed { role, .. }
            | Self::Progress { role, .. }
            | Self::FileNames { role, .. } => *role,
        }
    }

    /// 返回发送给 Tauri 前端的最终事件名
    ///
    /// 事件格式：
    /// `transfer:<role>:<state>`
    ///
    /// 示例：
    /// - `transfer:sender:started`
    /// - `transfer:receiver:progress`
    pub fn event_name(&self) -> String {
        format!("transfer:{}:{}", self.role().as_str(), self.state())
    }
}

/// 传输事件所属的角色（发送端 / 接收端）。
///
/// 用于区分事件来自哪一侧，
/// 前端与 CLI 可以据此展示不同视角的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 数据发送方
    Sender,
    /// 数据接收方
    Receiver,
}

impl Role {
    /// 用于事件字符串拼接（Tauri 前端）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }
}

/// 应用层句柄：可选包装的共享 `EventEmitter`。
///
/// 使用 `None` 表示不发射任何事件（例如在测试或禁止进度时）。
pub type AppHandle = Option<Arc<dyn EventEmitter>>;

/// 安全地向前端发送事件。
///
/// 若未配置事件发射器或发送失败，将被忽略。
pub fn emit_event(app: &AppHandle, event: &TransferEvent) {
    if let Some(handle) = app {
        handle.emit(event);
    }
}