//! 事件协议和生命周期校验测试。

use super::{Role, TransferEventData, TransferEventEnvelope, TransferPhase, TransferSessionId};
use std::str::FromStr;

pub(super) fn stream_event(
    session_id: &str,
    sequence: u64,
    event: TransferEventData,
) -> TransferEventEnvelope {
    TransferEventEnvelope::new(
        TransferSessionId::from_str(session_id).expect("valid session ID"),
        sequence,
        1_786_982_400_000,
        Role::Receiver,
        TransferPhase::Transferring,
        event,
    )
}

#[allow(deprecated)]
mod schema;
#[allow(deprecated)]
mod validator;
