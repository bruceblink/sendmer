use super::context::{ReceiveArtifacts, ReceiveContext};
use super::shutdown::{close_receive_endpoint, remove_temp_receive_dir};
use crate::core::recovery::RecoveryTerminalState;
use crate::core::results::ReceiveResult;
use crate::core::shutdown::with_shutdown_deadline;
use iroh_blobs::api::Store;
use std::path::PathBuf;

pub(super) fn finalize_failed_receive(
    primary_error: anyhow::Error,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Error {
    if let Err(error) = &cleanup_result {
        tracing::warn!(error = %error, "failed to cleanup receive context after error");
    }
    primary_error
}

pub(super) async fn cleanup_failed_receive(
    context: &mut ReceiveContext,
    terminal: RecoveryTerminalState,
) -> anyhow::Result<()> {
    #[cfg(not(feature = "persistent-recovery"))]
    let _ = terminal;
    #[cfg(feature = "persistent-recovery")]
    context.refresh_recovery(None).await;
    let endpoint_cleanup_result = close_receive_endpoint(&context.endpoint).await;
    let store_cleanup_result =
        shutdown_receive_store_and_cleanup_staging(&context.db, &mut context.active_staging_dir)
            .await;
    let preserve_uncached_storage =
        should_preserve_uncached_storage(cache_lease_present(context), &store_cleanup_result);
    let shutdown_result = combine_cleanup_results(endpoint_cleanup_result, store_cleanup_result);
    #[cfg(feature = "persistent-recovery")]
    if let Some(lease) = &context.cache_lease {
        lease.finish_recovery(terminal);
    }
    let storage_cleanup_result =
        finalize_receive_storage_after_store_cleanup(context, false, preserve_uncached_storage)
            .await;
    finalize_cleanup(shutdown_result, storage_cleanup_result)
}

pub(super) const fn should_preserve_uncached_storage(
    cache_lease_present: bool,
    store_cleanup_result: &anyhow::Result<()>,
) -> bool {
    !cache_lease_present && store_cleanup_result.is_err()
}

pub(super) async fn finish_receive(
    context: &mut ReceiveContext,
    artifacts: ReceiveArtifacts,
) -> anyhow::Result<ReceiveResult> {
    // The successful export only copies verified blobs; this session owns the
    // store exclusively and the default FsStore has GC disabled. Reuse only a
    // complete checkpoint committed in the current timestamp second. Otherwise
    // refresh as before, including when the previous barrier or journal failed.
    #[cfg(feature = "persistent-recovery")]
    if !context
        .cache_lease
        .as_ref()
        .is_some_and(|lease| lease.has_fresh_complete_checkpoint())
    {
        context.refresh_recovery(None).await;
    } else {
        tracing::debug!("reuse fresh complete receive recovery checkpoint");
    }
    let endpoint_cleanup_result = close_receive_endpoint(&context.endpoint).await;
    let store_cleanup_result =
        shutdown_receive_store_and_cleanup_staging(&context.db, &mut context.active_staging_dir)
            .await;
    let preserve_uncached_storage =
        should_preserve_uncached_storage(cache_lease_present(context), &store_cleanup_result);
    let shutdown_result = combine_cleanup_results(endpoint_cleanup_result, store_cleanup_result);
    #[cfg(feature = "persistent-recovery")]
    if let Some(lease) = &context.cache_lease {
        let terminal = if shutdown_result.is_ok() {
            RecoveryTerminalState::Completed
        } else {
            RecoveryTerminalState::Failed
        };
        lease.finish_recovery(terminal);
    }
    let storage_cleanup_result = finalize_receive_storage_after_store_cleanup(
        context,
        shutdown_result.is_ok(),
        preserve_uncached_storage,
    )
    .await;
    finalize_cleanup(shutdown_result, storage_cleanup_result)?;

    Ok(ReceiveResult {
        message: format!(
            "Downloaded {} files, {} bytes",
            artifacts.total_files, artifacts.payload_size
        ),
        file_path: artifacts.root_item_path,
    })
}

/// Drain submitted export work, shut down the store, then remove its tracked staging tree.
///
/// iroh-blobs dispatches exports on its own runtime; `Store::shutdown` only forwards to its
/// metadata database. `wait_idle` is therefore required while an export owns a staging path,
/// and cleanup runs after both operations have completed.
pub(super) async fn shutdown_receive_store_and_cleanup_staging(
    db: &Store,
    active_staging_dir: &mut Option<PathBuf>,
) -> anyhow::Result<()> {
    let idle_result = if active_staging_dir.is_some() {
        with_shutdown_deadline("receiver store wait_idle", async {
            db.wait_idle().await.map_err(anyhow::Error::from)
        })
        .await
    } else {
        Ok(())
    };
    let shutdown_result = with_shutdown_deadline("receiver store shutdown", async {
        db.shutdown().await.map_err(anyhow::Error::from)
    })
    .await;
    // 任一存储关闭步骤超时后，后台导出任务的所有权仍不确定。保留 staging
    // 目录，避免在任务仍访问文件时删除目录，后续由缓存清理流程处理残留。
    let staging_cleanup_result = if idle_result.is_ok() && shutdown_result.is_ok() {
        cleanup_active_staging_dir(active_staging_dir).await
    } else {
        tracing::warn!(
            error = ?idle_result.as_ref().err().or_else(|| shutdown_result.as_ref().err()),
            "skip staging cleanup after receiver store shutdown failure"
        );
        Ok(())
    };
    finalize_cleanup(
        combine_cleanup_results(idle_result, shutdown_result),
        staging_cleanup_result,
    )
}

/// Remove the active export staging directory after the blob store has stopped its export tasks.
async fn cleanup_active_staging_dir(
    active_staging_dir: &mut Option<PathBuf>,
) -> anyhow::Result<()> {
    let Some(path) = active_staging_dir.as_ref() else {
        return Ok(());
    };
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => {
            *active_staging_dir = None;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            *active_staging_dir = None;
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Preserve persistent data after failure, but remove it after a clean export.
#[cfg_attr(
    not(feature = "persistent-recovery"),
    allow(
        clippy::needless_pass_by_ref_mut,
        reason = "the persistent-cache build consumes the lease"
    )
)]
async fn finalize_receive_storage(
    context: &mut ReceiveContext,
    receive_succeeded: bool,
) -> anyhow::Result<()> {
    #[cfg(feature = "persistent-recovery")]
    if let Some(lease) = context.cache_lease.take() {
        return if receive_succeeded {
            lease.remove().await
        } else {
            lease.preserve()
        };
    }
    #[cfg(not(feature = "persistent-recovery"))]
    let _ = receive_succeeded;
    remove_temp_receive_dir(&context.iroh_data_dir).await
}

async fn finalize_receive_storage_after_store_cleanup(
    context: &mut ReceiveContext,
    receive_succeeded: bool,
    preserve_uncached_storage: bool,
) -> anyhow::Result<()> {
    if preserve_uncached_storage {
        // 存储关闭超时时保留无缓存目录，避免仍在运行的后台任务继续访问已删除的文件。
        tracing::warn!("preserve uncached receive storage after store shutdown failure");
        Ok(())
    } else {
        finalize_receive_storage(context, receive_succeeded).await
    }
}

pub(super) fn finalize_cleanup(
    shutdown_result: anyhow::Result<()>,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    if let Err(error) = &cleanup_result {
        tracing::warn!(error = %error, "failed to clean temporary receive dir");
    }
    shutdown_result
}

/// 合并有序关闭步骤的结果，保留第一个失败步骤及其错误链。
fn combine_cleanup_results(
    first: anyhow::Result<()>,
    second: anyhow::Result<()>,
) -> anyhow::Result<()> {
    match (first, second) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(first_error), Err(second_error)) => Err(first_error.context(format!(
            "subsequent cleanup step also failed: {second_error:#}"
        ))),
    }
}

#[cfg(feature = "persistent-recovery")]
const fn cache_lease_present(context: &ReceiveContext) -> bool {
    context.cache_lease.is_some()
}

#[cfg(not(feature = "persistent-recovery"))]
const fn cache_lease_present(_context: &ReceiveContext) -> bool {
    false
}
