use std::sync::Arc;

use super::{CompletionStatus, ProviderProgressTracker, TransferEventEmitter, TransferId};
use crate::core::types::EntryType;
use tokio::sync::{Mutex, watch};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SenderTransferStatus {
    Idle,
    Started,
    Completed,
    Aborted,
    /// The sender's configured fixed lifetime elapsed and the share was closed.
    Expired,
}

#[derive(Clone)]
pub struct SenderProgressReporter {
    emitter: TransferEventEmitter,
    state: Arc<Mutex<SenderProgressState>>,
    status_tx: watch::Sender<SenderTransferStatus>,
}

struct SenderProgressState {
    tracker: ProviderProgressTracker,
}

impl SenderProgressReporter {
    pub fn new(
        emitter: TransferEventEmitter,
        entry_type: EntryType,
        status_tx: watch::Sender<SenderTransferStatus>,
    ) -> Self {
        Self {
            emitter,
            state: Arc::new(Mutex::new(SenderProgressState {
                tracker: ProviderProgressTracker::new(entry_type),
            })),
            status_tx,
        }
    }

    /// Publish telemetry without allowing a late provider callback to overwrite
    /// the sender's terminal `Expired` lifecycle state.
    fn publish_status(&self, status: SenderTransferStatus) {
        let _ = self.status_tx.send_if_modified(|current| {
            if *current == SenderTransferStatus::Expired || *current == status {
                false
            } else {
                *current = status;
                true
            }
        });
    }

    pub async fn on_request_received(&self, transfer_id: TransferId, total_file_size: u64) {
        let mut state = self.state.lock().await;
        state
            .tracker
            .on_request_started(transfer_id, total_file_size);
        drop(state);
        self.publish_status(SenderTransferStatus::Started);
    }

    pub async fn on_request_update(
        &self,
        transfer_id: TransferId,
        update: iroh_blobs::provider::events::RequestUpdate,
    ) {
        match update {
            iroh_blobs::provider::events::RequestUpdate::Started(_) => {}
            iroh_blobs::provider::events::RequestUpdate::Progress(m) => {
                let mut state = self.state.lock().await;
                if let Some((processed, total, speed)) =
                    state.tracker.on_progress(transfer_id, m.end_offset)
                {
                    self.emitter.emit_progress(processed, total, speed);
                }
            }
            iroh_blobs::provider::events::RequestUpdate::Completed(_) => {
                let quiet_period = {
                    let mut state = self.state.lock().await;
                    match state.tracker.on_request_completed(transfer_id) {
                        CompletionStatus::Completed => {
                            self.publish_status(SenderTransferStatus::Completed);
                            None
                        }
                        CompletionStatus::InProgress => None,
                        CompletionStatus::MoreRequestsArrivingSoon => {
                            Some(state.tracker.completion_quiet_period())
                        }
                    }
                };

                if let Some(quiet_period) = quiet_period {
                    tokio::time::sleep(quiet_period).await;

                    let mut state = self.state.lock().await;
                    if matches!(
                        state.tracker.evaluate_completion(),
                        CompletionStatus::Completed
                    ) {
                        self.publish_status(SenderTransferStatus::Completed);
                    }
                }
            }
            iroh_blobs::provider::events::RequestUpdate::Aborted(_) => {
                let mut state = self.state.lock().await;
                state.tracker.on_request_aborted(transfer_id);
            }
        }
    }
}
