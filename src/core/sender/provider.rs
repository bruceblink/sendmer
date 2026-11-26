use crate::core::progress::{
    SenderProgressReporter, SenderTransferStatus, TransferEventEmitter, TransferId,
};
use iroh_blobs::provider::events::{
    ConnectMode, EventMask, EventSender, RequestMode, ThrottleMode,
};
use n0_future::task::AbortOnDropHandle;
use std::collections::HashSet;
use std::num::NonZeroU64;
use std::time::Duration;
use tokio::select;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::Instant;

/// 从提供者事件流中读取进度信息并使用ProviderProgressTracker进行跟踪。
///
/// 该函数使用ProviderProgressTracker来管理多个并发传输的进度，并根据完成状态发射相应的事件。
pub(super) async fn show_provide_progress_with_provider_tracker(
    mut recv: mpsc::Receiver<iroh_blobs::provider::events::ProviderMessage>,
    event_emitter: TransferEventEmitter,
    total_file_size: u64,
    entry_type: crate::core::types::EntryType,
    transfer_status_tx: watch::Sender<SenderTransferStatus>,
    control: ProviderProgressControl,
) -> anyhow::Result<()> {
    let ProviderProgressControl {
        max_upload_rate_bytes_per_sec,
        max_receivers,
        mut shutdown_signal_rx,
    } = control;
    let reporter = SenderProgressReporter::new(event_emitter, entry_type, transfer_status_tx);
    let request_task_limit = std::sync::Arc::new(Semaphore::new(PROVIDER_PROGRESS_TASK_LIMIT));
    let upload_rate_limiter = max_upload_rate_bytes_per_sec
        .map(UploadRateLimiter::new)
        .map(std::sync::Arc::new);
    let mut receiver_admission = ReceiverAdmission::new(max_receivers);
    let mut request_tasks = JoinSet::new();
    let mut throttle_tasks = JoinSet::new();

    while let Some(item) = select! {
        biased;
        _ = shutdown_signal_rx.changed() => None,
        item = recv.recv() => item,
    } {
        while request_tasks.try_join_next().is_some() {}
        while throttle_tasks.try_join_next().is_some() {}

        match item {
            iroh_blobs::provider::events::ProviderMessage::ClientConnected(msg) => {
                let result = if receiver_admission.admit(msg.connection_id) {
                    Ok(())
                } else {
                    Err(iroh_blobs::provider::events::AbortReason::RateLimited)
                };
                let _ = msg.tx.send(result).await;
            }
            iroh_blobs::provider::events::ProviderMessage::ClientConnectedNotify(_msg) => {}
            iroh_blobs::provider::events::ProviderMessage::ConnectionClosed(msg) => {
                receiver_admission.release(msg.connection_id);
            }
            iroh_blobs::provider::events::ProviderMessage::GetRequestReceivedNotify(msg) => {
                let transfer_id = TransferId::new(msg.connection_id, msg.request_id);
                let Some(permit) = try_acquire_provider_progress_permit(&request_task_limit) else {
                    // 进度更新属于诊断信息。达到并发上限时丢弃本次更新，不能阻塞
                    // 主事件循环，否则会延迟连接关闭、限速响应和 sender shutdown。
                    tracing::debug!(
                        connection_id = msg.connection_id,
                        request_id = msg.request_id,
                        "drop provider progress updates after task limit"
                    );
                    continue;
                };
                reporter
                    .on_request_received(transfer_id, total_file_size)
                    .await;

                let reporter_clone = reporter.clone();
                let mut rx = msg.rx;
                let mut shutdown_signal_rx = shutdown_signal_rx.clone();
                request_tasks.spawn(async move {
                    let _permit = permit;
                    loop {
                        tokio::select! {
                            biased;
                            _ = shutdown_signal_rx.changed() => break,
                            update = rx.recv() => match update {
                                Ok(Some(update)) => {
                                    reporter_clone.on_request_update(transfer_id, update).await;
                                }
                                _ => break,
                            },
                        }
                    }
                });
            }
            iroh_blobs::provider::events::ProviderMessage::Throttle(msg) => {
                if let Some(limiter) = upload_rate_limiter.clone() {
                    let mut shutdown_signal_rx = shutdown_signal_rx.clone();
                    throttle_tasks.spawn(async move {
                        if limiter
                            .wait_for_chunk_or_shutdown(msg.size, &mut shutdown_signal_rx)
                            .await
                        {
                            let _ = msg.tx.send(Ok(())).await;
                        }
                    });
                } else {
                    let _ = msg.tx.send(Ok(())).await;
                }
            }
            _ => {
                // Handle other message types that we don't need to track
            }
        }
    }

    // 请求进度回调可能无限等待对端更新。进度任务返回前中止并等待这些任务，
    // 确保 close/cancel 不会留下晚到的状态更新。
    request_tasks.abort_all();
    while request_tasks.join_next().await.is_some() {}

    // Outstanding throttle waits can be arbitrarily long at very low rates. Abort them
    // when the provider channel closes so sender shutdown never waits for pacing sleeps.
    throttle_tasks.abort_all();
    while throttle_tasks.join_next().await.is_some() {}

    Ok(())
}

/// Track active provider connections and reject new ones after the configured ceiling.
///
/// The provider event stream is processed serially, so this state machine makes admission
/// decisions without holding a lock across asynchronous response sends.
pub(super) struct ReceiverAdmission {
    max_receivers: Option<NonZeroU64>,
    active_connections: HashSet<u64>,
}

impl ReceiverAdmission {
    pub(super) fn new(max_receivers: Option<NonZeroU64>) -> Self {
        Self {
            max_receivers,
            active_connections: HashSet::new(),
        }
    }

    /// Admit a connection unless it would exceed the configured active-peer limit.
    pub(super) fn admit(&mut self, connection_id: u64) -> bool {
        if self.active_connections.contains(&connection_id) {
            return true;
        }
        if let Some(max_receivers) = self.max_receivers {
            let active_connections =
                u64::try_from(self.active_connections.len()).expect("usize fits into u64");
            if active_connections >= max_receivers.get() {
                return false;
            }
        }
        self.active_connections.insert(connection_id);
        true
    }

    /// Release a connection slot after the provider reports that it closed.
    pub(super) fn release(&mut self, connection_id: u64) {
        self.active_connections.remove(&connection_id);
    }
}

/// Serialize payload chunk reservations so every receiver shares one upload budget.
pub(super) struct UploadRateLimiter {
    bytes_per_second: NonZeroU64,
    next_allowed_at: tokio::sync::Mutex<Instant>,
}

impl UploadRateLimiter {
    pub(super) fn new(bytes_per_second: NonZeroU64) -> Self {
        Self {
            bytes_per_second,
            next_allowed_at: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    /// Reserve the next global pacing slot without holding the mutex while a task sleeps.
    pub(super) async fn reserve(&self, bytes: u64) -> Instant {
        let now = Instant::now();
        let mut next_allowed_at = self.next_allowed_at.lock().await;
        let allowed_at = (*next_allowed_at).max(now);
        *next_allowed_at = allowed_at
            .checked_add(upload_delay(bytes, self.bytes_per_second))
            .unwrap_or(allowed_at);
        allowed_at
    }

    /// Wait for a pacing slot, returning false when sender shutdown cancels the pending wait.
    pub(super) async fn wait_for_chunk_or_shutdown(
        &self,
        bytes: u64,
        shutdown_signal_rx: &mut watch::Receiver<bool>,
    ) -> bool {
        if *shutdown_signal_rx.borrow() {
            return false;
        }

        let deadline = self.reserve(bytes).await;
        tokio::select! {
            biased;
            _ = shutdown_signal_rx.changed() => false,
            _ = tokio::time::sleep_until(deadline) => true,
        }
    }
}

/// Convert a payload chunk into a rounded-up pacing interval that cannot exceed the configured rate.
pub(super) fn upload_delay(bytes: u64, bytes_per_second: NonZeroU64) -> Duration {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;

    let byte_nanoseconds = u128::from(bytes) * NANOS_PER_SECOND;
    let rate = u128::from(bytes_per_second.get());
    let nanoseconds =
        byte_nanoseconds / rate + u128::from((!byte_nanoseconds.is_multiple_of(rate)) as u8);
    let seconds = nanoseconds / NANOS_PER_SECOND;
    if seconds > u128::from(u64::MAX) {
        return Duration::MAX;
    }

    Duration::new(seconds as u64, (nanoseconds % NANOS_PER_SECOND) as u32)
}

pub(super) fn create_event_sender(
    progress_tx: mpsc::Sender<iroh_blobs::provider::events::ProviderMessage>,
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
) -> EventSender {
    EventSender::new(
        progress_tx,
        provider_event_mask(max_upload_rate_bytes_per_sec, max_receivers),
    )
}

/// Build the provider subscription mask for optional throttling and admission control.
pub(super) const fn provider_event_mask(
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
) -> EventMask {
    EventMask {
        connected: if max_receivers.is_some() {
            ConnectMode::Intercept
        } else {
            ConnectMode::Notify
        },
        get: RequestMode::NotifyLog,
        throttle: if max_upload_rate_bytes_per_sec.is_some() {
            ThrottleMode::Intercept
        } else {
            ThrottleMode::None
        },
        ..EventMask::DEFAULT
    }
}

pub(super) fn spawn_provider_progress_task(
    progress_rx: mpsc::Receiver<iroh_blobs::provider::events::ProviderMessage>,
    event_emitter: TransferEventEmitter,
    total_file_size: u64,
    entry_type: crate::core::types::EntryType,
    transfer_status_tx: watch::Sender<SenderTransferStatus>,
    control: ProviderProgressControl,
) -> AbortOnDropHandle<anyhow::Result<()>> {
    AbortOnDropHandle::new(tokio::spawn(show_provide_progress_with_provider_tracker(
        progress_rx,
        event_emitter,
        total_file_size,
        entry_type,
        transfer_status_tx,
        control,
    )))
}

/// Bundle provider limits and the sender shutdown signal passed to the progress task.
pub(super) struct ProviderProgressControl {
    pub(super) max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    pub(super) max_receivers: Option<NonZeroU64>,
    pub(super) shutdown_signal_rx: watch::Receiver<bool>,
}

/// 尝试取得一个进度跟踪任务配额，不在提供者事件循环中等待。
pub(super) fn try_acquire_provider_progress_permit(
    request_task_limit: &std::sync::Arc<Semaphore>,
) -> Option<tokio::sync::OwnedSemaphorePermit> {
    request_task_limit.clone().try_acquire_owned().ok()
}

const PROVIDER_PROGRESS_TASK_LIMIT: usize = 32;
