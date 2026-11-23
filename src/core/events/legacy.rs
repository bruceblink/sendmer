//! 旧事件别名和前端发射接口。

use super::payload::TransferEventEnvelope;
use super::types::Role;

/// Public event type emitted by sendmer v0.8 integrations.
pub type TransferEvent = TransferEventEnvelope;

/// 事件发射器接口。
///
/// 库代码通过该 trait 将 [`TransferEventEnvelope`]
/// 发送到 CLI / Tauri / GUI 等不同前端实现。
///
/// 设计约束：
/// - 不返回 `Result`
/// - 事件发送失败不得影响主流程
/// - 实现应尽量做到非阻塞
pub trait EventEmitter: Send + Sync {
    /// 发射一个传输事件。
    fn emit(&self, event: &TransferEventEnvelope);
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
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[deprecated(
    since = "0.8.0",
    note = "use TransferEventEnvelope and match TransferEventData instead"
)]
pub enum LegacyTransferEvent {
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

#[allow(deprecated)]
impl LegacyTransferEvent {
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
