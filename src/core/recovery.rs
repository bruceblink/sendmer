//! 传输恢复快照和已验证范围记录。
//!
//! 本模块只记录可复用的数据范围和会话终态。它不读取 CLI 状态，也不控制 iroh 连接。

use crate::core::events::{Role, TransferSessionId};
use serde::{Deserialize, Serialize};
use std::ops::Range;

/// 当前恢复快照格式版本。
pub const RECOVERY_SNAPSHOT_SCHEMA_VERSION: u16 = 1;

/// 允许复用的最后终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryTerminalState {
    Completed,
    Failed,
    Cancelled,
    Expired,
}

/// 一个传输会话的可迁移恢复记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoverySnapshot {
    pub schema_version: u16,
    pub role: Role,
    pub session_id: TransferSessionId,
    pub verified_ranges: Vec<Range<u64>>,
    pub processed_bytes: u64,
    pub last_terminal_state: Option<RecoveryTerminalState>,
}

impl RecoverySnapshot {
    /// 创建一个没有已验证范围的新快照。
    pub const fn new(role: Role, session_id: TransferSessionId) -> Self {
        Self {
            schema_version: RECOVERY_SNAPSHOT_SCHEMA_VERSION,
            role,
            session_id,
            verified_ranges: Vec::new(),
            processed_bytes: 0,
            last_terminal_state: None,
        }
    }

    /// 记录已验证的半开区间，并合并相邻或重叠区间。
    pub fn record_verified_range(&mut self, range: Range<u64>) {
        if range.start >= range.end {
            return;
        }
        self.verified_ranges.push(range);
        self.verified_ranges.sort_by_key(|item| item.start);
        let mut merged: Vec<Range<u64>> = Vec::with_capacity(self.verified_ranges.len());
        for range in self.verified_ranges.drain(..) {
            if let Some(last) = merged.last_mut()
                && range.start <= last.end
            {
                last.end = last.end.max(range.end);
                continue;
            }
            merged.push(range);
        }
        self.verified_ranges = merged;
    }

    /// 计算已验证范围的总字节数。
    pub fn verified_bytes(&self) -> u64 {
        self.verified_ranges
            .iter()
            .map(|range| range.end.saturating_sub(range.start))
            .fold(0, u64::saturating_add)
    }

    /// 更新已经处理的字节数。该字段不代表校验已经完成。
    pub fn record_progress(&mut self, processed_bytes: u64) {
        self.processed_bytes = self.processed_bytes.max(processed_bytes);
    }

    /// 写入一个终态并返回是否仍可用于失败后的恢复。
    pub const fn record_terminal_state(&mut self, state: RecoveryTerminalState) -> bool {
        self.last_terminal_state = Some(state);
        !matches!(
            state,
            RecoveryTerminalState::Completed | RecoveryTerminalState::Expired
        )
    }

    /// 判断快照是否可以作为后续接收的范围摘要。
    pub const fn can_resume(&self) -> bool {
        self.schema_version == RECOVERY_SNAPSHOT_SCHEMA_VERSION
            && !self.verified_ranges.is_empty()
            && !matches!(
                self.last_terminal_state,
                Some(RecoveryTerminalState::Completed | RecoveryTerminalState::Expired)
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{RECOVERY_SNAPSHOT_SCHEMA_VERSION, RecoverySnapshot, RecoveryTerminalState};
    use crate::core::events::{Role, TransferSessionId};

    #[test]
    fn verified_ranges_merge_and_report_bytes() {
        let mut snapshot = RecoverySnapshot::new(Role::Receiver, TransferSessionId::new());
        snapshot.record_verified_range(0..4);
        snapshot.record_verified_range(4..10);
        snapshot.record_verified_range(8..12);
        assert_eq!(snapshot.verified_ranges, vec![0..12]);
        assert_eq!(snapshot.verified_bytes(), 12);
        assert!(snapshot.can_resume());
    }

    #[test]
    fn completed_snapshot_is_not_resumable() {
        let mut snapshot = RecoverySnapshot::new(Role::Receiver, TransferSessionId::new());
        snapshot.record_verified_range(0..4);
        assert!(!snapshot.record_terminal_state(RecoveryTerminalState::Completed));
        assert!(!snapshot.can_resume());
    }

    #[test]
    fn snapshot_round_trips_with_schema_version() {
        let snapshot = RecoverySnapshot::new(Role::Sender, TransferSessionId::new());
        let encoded = serde_json::to_vec(&snapshot).expect("serialize recovery snapshot");
        let decoded: RecoverySnapshot =
            serde_json::from_slice(&encoded).expect("deserialize recovery snapshot");
        assert_eq!(decoded, snapshot);
        assert_eq!(decoded.schema_version, RECOVERY_SNAPSHOT_SCHEMA_VERSION);
    }
}
