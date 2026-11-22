//! 资源关闭辅助函数。
//!
//! 关闭网络路由和本地存储时必须设置上限。底层任务异常时，调用方仍然需要释放
//! 所有句柄，并保留超时或底层错误，避免把失败误报为成功。

use anyhow::Context;
use std::future::Future;
use std::time::Duration;

/// 单个资源关闭操作允许占用的最长时间。
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// 在固定期限内执行关闭操作，并保留底层错误上下文。
pub async fn with_shutdown_deadline<T, F>(label: &'static str, future: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    tokio::time::timeout(SHUTDOWN_TIMEOUT, future)
        .await
        .map_or_else(
            |_| {
                Err(anyhow::anyhow!(
                    "{label} timed out after {} seconds",
                    SHUTDOWN_TIMEOUT.as_secs()
                ))
            },
            |result| result.with_context(|| format!("{label} failed")),
        )
}

#[cfg(test)]
mod tests {
    use super::{SHUTDOWN_TIMEOUT, with_shutdown_deadline};
    use std::future::pending;

    #[tokio::test]
    async fn shutdown_deadline_preserves_operation_error() {
        let error = with_shutdown_deadline("test shutdown", async {
            Err::<(), _>(anyhow::anyhow!("underlying failure"))
        })
        .await
        .expect_err("operation should fail");

        let message = format!("{error:#}");
        assert!(message.contains("test shutdown failed"));
        assert!(message.contains("underlying failure"));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_deadline_reports_timeout() {
        let task = tokio::spawn(with_shutdown_deadline(
            "test shutdown",
            pending::<anyhow::Result<()>>(),
        ));
        tokio::time::advance(SHUTDOWN_TIMEOUT).await;
        let error = task
            .await
            .expect("deadline task should finish")
            .expect_err("pending shutdown should time out");

        assert!(error.to_string().contains("test shutdown timed out"));
    }
}
