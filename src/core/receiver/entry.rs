use super::cleanup::{cleanup_failed_receive, finalize_failed_receive, finish_receive};
use super::context::ReceiveContext;
use super::download::receive_once;
use super::errors::{emit_receive_failed, receive_failed_message, receive_failure};
use crate::core::events::{
    AppHandle, Role, TransferErrorCode, TransferPhase, classified_transfer_error,
    is_transfer_cancelled, transfer_cancelled_error,
};
use crate::core::filesystem::resolve_output_dir;
use crate::core::options::{ReceiveLimits, ReceiveOptions};
use crate::core::progress::TransferEventEmitter;
use crate::core::results::ReceiveResult;
use iroh_blobs::ticket::BlobTicket;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use tokio::select;
use tracing::info;

/// 下载并导出由 `ticket_str` 指定的数据到本地目录，不发射事件。
pub async fn receive(ticket_str: String, options: ReceiveOptions) -> anyhow::Result<ReceiveResult> {
    receive_with_limits_and_cancellation(ticket_str, options, ReceiveLimits::default(), None, None)
        .await
}

/// 下载并导出由 `ticket_str` 指定的数据，并向事件消费者发送生命周期事件。
///
/// - `ticket_str`：连接票据字符串。
/// - `options`：接收选项（输出目录、转发模式等）。
/// - `app_handle`：可选的事件发射器句柄，用于 UI/CLI 上报进度与文件名等信息。
pub async fn receive_with_events(
    ticket_str: String,
    options: ReceiveOptions,
    app_handle: AppHandle,
) -> anyhow::Result<ReceiveResult> {
    receive_with_limits(ticket_str, options, ReceiveLimits::default(), app_handle).await
}

/// 使用资源配额接收票据。
///
/// `ReceiveOptions` 保持原有字段不变，配额通过独立参数传入，避免破坏已有的
/// 公开结构体字面量调用。未设置配额时请传入 `ReceiveLimits::default()`。
pub async fn receive_with_limits(
    ticket_str: String,
    options: ReceiveOptions,
    limits: ReceiveLimits,
    app_handle: AppHandle,
) -> anyhow::Result<ReceiveResult> {
    receive_with_limits_and_cancellation(ticket_str, options, limits, app_handle, None).await
}

/// Download a ticket while allowing the caller to request graceful cleanup.
///
/// The optional watch receiver is intentionally separate from `ReceiveOptions` so existing
/// callers keep compiling. Cancellation follows the same cleanup path as Ctrl+C: the receive
/// endpoint, store, and temporary directory are closed before the future returns.
pub async fn receive_with_cancellation(
    ticket_str: String,
    options: ReceiveOptions,
    app_handle: AppHandle,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> anyhow::Result<ReceiveResult> {
    receive_with_events_and_cancellation(ticket_str, options, app_handle, cancellation).await
}

/// Receive with events and caller-requested cancellation using default resource limits.
pub async fn receive_with_events_and_cancellation(
    ticket_str: String,
    options: ReceiveOptions,
    app_handle: AppHandle,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> anyhow::Result<ReceiveResult> {
    receive_with_limits_and_cancellation(
        ticket_str,
        options,
        ReceiveLimits::default(),
        app_handle,
        cancellation,
    )
    .await
}

/// 使用资源配额接收票据，并允许调用方请求有序取消。
pub async fn receive_with_limits_and_cancellation(
    ticket_str: String,
    options: ReceiveOptions,
    limits: ReceiveLimits,
    app_handle: AppHandle,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> anyhow::Result<ReceiveResult> {
    let event_emitter = TransferEventEmitter::new(app_handle, Role::Receiver);
    event_emitter.emit_started(TransferPhase::Preparing);
    let mut cancellation = cancellation;
    let (mut context, output_dir) =
        prepare_receive_context(&ticket_str, &options, limits, &cancellation, &event_emitter)
            .await?;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    if cancellation_requested(cancellation.as_ref()) || ctrl_c_requested(ctrl_c.as_mut()).await {
        let error = finalize_failed_receive(
            transfer_cancelled_error(),
            cleanup_failed_receive(&mut context).await,
        );
        event_emitter.emit_cancelled();
        return Err(error);
    }
    let result = run_receive_transfer(
        &mut context,
        &output_dir,
        &event_emitter,
        cancellation.take(),
        ctrl_c,
    )
    .await?;
    info!("receive completed");
    Ok(result)
}

async fn prepare_receive_context(
    ticket_str: &str,
    options: &ReceiveOptions,
    limits: ReceiveLimits,
    cancellation: &Option<tokio::sync::watch::Receiver<bool>>,
    event_emitter: &TransferEventEmitter,
) -> anyhow::Result<(ReceiveContext, PathBuf)> {
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    if cancellation_requested(cancellation.as_ref()) || ctrl_c_requested(ctrl_c.as_mut()).await {
        event_emitter.emit_cancelled();
        return Err(transfer_cancelled_error());
    }
    if let Err(error) = limits.validate().map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::InvalidInput,
            TransferPhase::Preparing,
            false,
            "invalid receive resource limits",
        )
    }) {
        emit_receive_failed(event_emitter, &error);
        return Err(error);
    }
    let ticket = match BlobTicket::from_str(ticket_str).map_err(|error| {
        receive_failure(
            error.into(),
            TransferErrorCode::InvalidInput,
            TransferPhase::Preparing,
            false,
            "invalid receive ticket",
        )
    }) {
        Ok(ticket) => ticket,
        Err(error) => {
            emit_receive_failed(event_emitter, &error);
            return Err(error);
        }
    };
    info!(
        hash = %ticket.hash(),
        relay_addrs = ticket.addr().relay_urls().count(),
        ip_addrs = ticket.addr().ip_addrs().count(),
        "starting receive"
    );
    let output_dir = match resolve_output_dir(options.output_dir.clone()).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Preparing,
            false,
            "unable to resolve receive output directory",
        )
    }) {
        Ok(output_dir) => output_dir,
        Err(error) => {
            emit_receive_failed(event_emitter, &error);
            return Err(error);
        }
    };
    if cancellation_requested(cancellation.as_ref()) || ctrl_c_requested(ctrl_c.as_mut()).await {
        event_emitter.emit_cancelled();
        return Err(transfer_cancelled_error());
    }
    let mut context = match ReceiveContext::prepare(ticket, options, limits).await {
        Ok(context) => context,
        Err(error) => {
            emit_receive_failed(event_emitter, &error);
            return Err(error);
        }
    };
    if cancellation_requested(cancellation.as_ref()) || ctrl_c_requested(ctrl_c.as_mut()).await {
        let error = finalize_failed_receive(
            transfer_cancelled_error(),
            cleanup_failed_receive(&mut context).await,
        );
        event_emitter.emit_cancelled();
        return Err(error);
    }
    Ok((context, output_dir))
}

async fn run_receive_transfer<F>(
    context: &mut ReceiveContext,
    output_dir: &Path,
    event_emitter: &TransferEventEmitter,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
    mut ctrl_c: Pin<&mut F>,
) -> anyhow::Result<ReceiveResult>
where
    F: Future<Output = std::io::Result<()>> + ?Sized,
{
    let receive_result = select! {
        biased;
        _ = wait_for_cancellation(cancellation) => {
            tracing::warn!("operation cancelled by caller");
            Err(transfer_cancelled_error())
        }
        _ = ctrl_c.as_mut() => {
            tracing::warn!("operation cancelled by user");
            Err(transfer_cancelled_error())
        }
        result = receive_once(context, output_dir, event_emitter.clone()) => result,
    };
    let artifacts = match receive_result {
        Ok(artifacts) => artifacts,
        Err(error) => {
            let cancelled = is_transfer_cancelled(&error);
            if let Some(details) = classified_transfer_error(&error) {
                tracing::error!(
                    ?details.code,
                    ?details.phase,
                    retryable = details.retryable,
                    cancelled,
                    "download operation failed"
                );
            } else {
                tracing::error!(cancelled, "download operation failed");
            }
            let error = finalize_failed_receive(error, cleanup_failed_receive(context).await);
            if cancelled {
                event_emitter.emit_cancelled();
                return Err(error);
            }
            let message = receive_failed_message(&error);
            let error = error.context(message);
            emit_receive_failed(event_emitter, &error);
            return Err(error);
        }
    };
    let result = match finish_receive(context, artifacts).await {
        Ok(result) => result,
        Err(error) => {
            let error = receive_failure(
                error,
                TransferErrorCode::Internal,
                TransferPhase::Finalizing,
                false,
                "unable to finalize received data",
            );
            let message = receive_failed_message(&error);
            let error = error.context(message);
            emit_receive_failed(event_emitter, &error);
            return Err(error);
        }
    };
    event_emitter.emit_completed();
    Ok(result)
}

/// Waits until the optional cancellation watch channel is set to true.
///
/// A missing receiver becomes a pending future, so the normal `receive` API keeps its existing
/// Ctrl+C-only behavior while the GPUI adapter can opt into graceful cancellation.
pub(super) fn cancellation_requested(
    cancellation: Option<&tokio::sync::watch::Receiver<bool>>,
) -> bool {
    cancellation.is_some_and(|receiver| *receiver.borrow())
}

/// 非阻塞检查 Ctrl+C，避免准备阶段在信号尚未到达时等待永久 pending future。
async fn ctrl_c_requested<F>(signal: Pin<&mut F>) -> bool
where
    F: Future<Output = std::io::Result<()>> + ?Sized,
{
    tokio::select! {
        biased;
        _ = signal => true,
        _ = tokio::task::yield_now() => false,
    }
}

pub(super) async fn wait_for_cancellation(
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) {
    let Some(mut cancellation) = cancellation else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *cancellation.borrow() {
            return;
        }
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}
