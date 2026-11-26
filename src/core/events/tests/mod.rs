//! 事件协议和生命周期校验测试。

use super::{Role, TransferEventData, TransferEventEnvelope, TransferPhase, TransferSessionId};
use std::str::FromStr;

pub(super) fn stream_event(
    session_id: &str,
    sequence: u64,
    event: TransferEventData,
) -> TransferEventEnvelope {
    let phase = match &event {
        TransferEventData::Completed => TransferPhase::Finalizing,
        TransferEventData::FileNames { .. } => TransferPhase::Metadata,
        TransferEventData::Failed { error } => error.phase,
        _ => TransferPhase::Transferring,
    };
    TransferEventEnvelope::new(
        TransferSessionId::from_str(session_id).expect("valid session ID"),
        sequence,
        1_786_982_400_000,
        Role::Receiver,
        phase,
        event,
    )
}

mod lifecycle;
#[allow(deprecated)]
mod schema;
#[allow(deprecated)]
mod validator;
