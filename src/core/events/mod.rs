//! 传输事件模型、事件校验和事件发射接口。
//!
//! 子模块只负责事件数据和生命周期规则。网络、文件系统和 CLI 输出由上层模块负责。

mod legacy;
mod payload;
mod types;
mod validator;

#[allow(deprecated)]
pub use legacy::{EventEmitter, LegacyTransferEvent, TransferEvent};
pub use payload::{TransferEventData, TransferEventEnvelope};
pub use types::{
    ParseTransferSessionIdError, Role, TRANSFER_EVENT_SCHEMA_VERSION, TransferError,
    TransferErrorCode, TransferPhase, TransferSessionId,
};
pub use validator::{TransferEventStreamError, TransferEventStreamValidator};

pub(crate) use payload::is_safe_event_file_name;
pub(crate) use types::{
    classified_transfer_error, classify_transfer_error, is_transfer_cancelled,
    transfer_cancelled_error,
};

use std::sync::Arc;

/// 可选的事件消费句柄。没有句柄时，核心传输只执行控制流程。
pub type AppHandle = Option<Arc<dyn EventEmitter>>;

/// 将事件交给可选消费者。消费者失败不会改变传输控制流程。
pub fn emit_event(app: &AppHandle, event: &TransferEventEnvelope) {
    if let Some(handle) = app {
        handle.emit(event);
    }
}

#[cfg(test)]
mod tests;
