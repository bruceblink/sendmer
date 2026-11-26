use super::context::DownloadPlan;
use crate::core::options::ReceiveLimits;

/// 检查远端声明的文件数量和 payload 总字节数是否符合本地配额。
pub(super) fn validate_receive_limits(
    total_files: u64,
    payload_size: u64,
    limits: ReceiveLimits,
) -> anyhow::Result<()> {
    if let Some(max_files) = limits.max_files {
        anyhow::ensure!(
            total_files <= max_files.get(),
            "received transfer contains {total_files} files, exceeding the local limit of {}",
            max_files
        );
    }
    if let Some(max_total_size_bytes) = limits.max_total_size_bytes {
        anyhow::ensure!(
            payload_size <= max_total_size_bytes.get(),
            "received transfer contains {payload_size} bytes, exceeding the local limit of {}",
            max_total_size_bytes
        );
    }
    Ok(())
}

/// 在每次重连前检查已保存的数据，防止恢复过程越过接收端配额。
pub(super) fn validate_partial_receive_limits(
    local_bytes: u64,
    local_children: Option<u64>,
    plan: &DownloadPlan,
    limits: ReceiveLimits,
) -> anyhow::Result<()> {
    if let Some(max_files) = limits.max_files
        && let Some(children) = local_children
    {
        let files = completed_local_total_files(children);
        anyhow::ensure!(
            files <= max_files.get(),
            "received transfer already contains {files} files, exceeding the local limit of {}",
            max_files
        );
    }
    if let Some(max_total_size_bytes) = limits.max_total_size_bytes {
        let protocol_overhead = plan.transfer_total.saturating_sub(plan.payload_size);
        let max_transfer_bytes = max_total_size_bytes.get().saturating_add(protocol_overhead);
        anyhow::ensure!(
            local_bytes <= max_transfer_bytes,
            "received transfer already contains more than the local byte limit of {}",
            max_total_size_bytes
        );
    }
    Ok(())
}

pub(super) const fn completed_local_total_files(children: u64) -> u64 {
    children.saturating_sub(1)
}

pub(super) fn completed_local_total_files_from_children(
    children: Option<u64>,
) -> anyhow::Result<u64> {
    children
        .map(completed_local_total_files)
        .ok_or_else(|| anyhow::anyhow!("local complete state missing collection children"))
}

/// Calculate the transfer-byte ceiling that includes collection protocol overhead.
pub(super) fn max_transfer_limit(plan: &DownloadPlan, limits: ReceiveLimits) -> Option<u64> {
    limits.max_total_size_bytes.map(|limit| {
        limit
            .get()
            .saturating_add(plan.transfer_total.saturating_sub(plan.payload_size))
    })
}
