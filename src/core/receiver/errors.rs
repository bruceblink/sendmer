use crate::core::events::{
    TransferError, TransferErrorCode, TransferPhase, classified_transfer_error,
    classify_transfer_error,
};
use crate::core::progress::TransferEventEmitter;
use iroh_blobs::get::GetError;

/// Format a user-facing failure while keeping both stage context and root cause.
pub(super) fn receive_failed_message(error: &anyhow::Error) -> String {
    let details = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    format!("error: {details}")
}

pub(super) fn emit_receive_failed(emitter: &TransferEventEmitter, error: &anyhow::Error) {
    if let Some(details) = classified_transfer_error(error) {
        emitter.emit_failed(details);
    } else {
        emitter.emit_internal_failure("receive failed");
    }
}

/// Pair an internal receive error with the stable details exposed to event consumers.
pub(super) fn receive_failure(
    error: anyhow::Error,
    code: TransferErrorCode,
    phase: TransferPhase,
    retryable: bool,
    message: &'static str,
) -> anyhow::Error {
    classify_transfer_error(error, TransferError::new(code, phase, retryable, message))
}

/// Classify protocol failures without asking consumers to parse `GetError` text.
pub(super) fn receive_protocol_failure(
    error: anyhow::Error,
    phase: TransferPhase,
    fallback_message: &'static str,
) -> anyhow::Error {
    let (code, retryable, message) = match error.downcast_ref::<GetError>() {
        Some(GetError::BadRequest { .. }) => (
            TransferErrorCode::RemoteRejected,
            false,
            "sender rejected the transfer request",
        ),
        Some(GetError::LocalFailure { .. }) => (
            TransferErrorCode::Internal,
            false,
            "local transfer processing failed",
        ),
        _ => (
            TransferErrorCode::TransferInterrupted,
            true,
            fallback_message,
        ),
    };
    receive_failure(error, code, phase, retryable, message)
}

/// 记录 `GetError` 的稳定类别并原样返回，便于上层保留结构化失败语义。
pub(super) fn show_get_error(e: GetError) -> GetError {
    log_get_error(&e);
    e
}

fn log_get_error(e: &GetError) {
    match e {
        GetError::InitialNext { .. }
        | GetError::ConnectedNext { .. }
        | GetError::AtBlobHeaderNext { .. } => {
            log_get_error_connection(e);
        }
        GetError::Decode { .. } | GetError::IrpcSend { .. } => {
            log_get_error_decode_or_irpc(e);
        }
        GetError::AtClosingNext { .. }
        | GetError::BadRequest { .. }
        | GetError::LocalFailure { .. } => {
            log_get_error_misc(e);
        }
    }
}

fn log_get_error_connection(e: &GetError) {
    match e {
        GetError::InitialNext { .. } => tracing::error!("initial connection error"),
        GetError::ConnectedNext { .. } => tracing::error!("connected error"),
        GetError::AtBlobHeaderNext { .. } => tracing::error!("reading blob header error"),
        _ => {}
    }
}

fn log_get_error_decode_or_irpc(e: &GetError) {
    match e {
        GetError::Decode { .. } => tracing::error!("decoding error"),
        GetError::IrpcSend { .. } => tracing::error!("error sending over irpc"),
        _ => {}
    }
}

fn log_get_error_misc(e: &GetError) {
    match e {
        GetError::AtClosingNext { .. } => tracing::error!("error at closing"),
        GetError::BadRequest { .. } => tracing::error!("bad request"),
        GetError::LocalFailure { .. } => tracing::error!("local failure"),
        _ => {}
    }
}
