use crate::core::types::AppHandle;
use tracing::warn;

pub fn emit_event(app_handle: &AppHandle, event_name: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event(event_name) {
            warn!("Failed to emit event {}: {}", event_name, e);
        }
    }
}

pub fn emit_event_with_payload(app_handle: &AppHandle, event_name: &str, payload: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event_with_payload(event_name, payload) {
            warn!("Failed to emit event {} with payload: {}", event_name, e);
        }
    }
}

pub fn emit_progress_event(
    app_handle: &AppHandle,
    event_name: &str,
    bytes_transferred: u64,
    total_bytes: u64,
    speed_bps: f64,
) {
    if let Some(handle) = app_handle {
        let speed_int = (speed_bps * 1000.0) as i64;
        let payload = format!("{}:{}:{}", bytes_transferred, total_bytes, speed_int);
        if let Err(e) = handle.emit_event_with_payload(event_name, &payload) {
            warn!("Failed to emit progress event: {}", e);
        }
    }
}
