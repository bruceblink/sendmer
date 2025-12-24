//! 进度与事件发射相关的工具与 CLI 辅助实现。
//!
//! 本模块包含用于向外部 `EventEmitter` 发射事件的便捷函数，
//! 以及一个命令行环境下的事件发射器实现 `CliEventEmitter`，
//! 用于在控制台显示文件传输进度条。

use crate::core::types::{AppHandle, EventEmitter};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::warn;

/// 如果提供了 `app_handle` 则发射一个不带负载的事件。
///
/// - `app_handle`：可选的事件发射句柄（`None` 表示禁用进度/事件显示）。
/// - `event_name`：事件名称，例如 `transfer-started`。
pub fn emit_event(app_handle: &AppHandle, event_name: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event(event_name) {
            warn!("Failed to emit event {}: {}", event_name, e);
        }
    }
}

/// 发射带字符串负载的事件（如果 `app_handle` 存在）。
///
/// 负载格式由调用方指定，CLI 发射器期望形如 `"bytes:total:speed_int"`。
pub fn emit_event_with_payload(app_handle: &AppHandle, event_name: &str, payload: &str) {
    if let Some(handle) = app_handle {
        if let Err(e) = handle.emit_event_with_payload(event_name, payload) {
            warn!("Failed to emit event {} with payload: {}", event_name, e);
        }
    }
}

/// 将进度信息序列化为负载并发射为事件。
///
/// - `bytes_transferred`：已传输字节数。
/// - `total_bytes`：总字节数（若未知可为 0）。
/// - `speed_bps`：传输速率，单位为字节/秒（会被乘以 1000 后转换为整型以保持精度）。
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

/// 命令行模式下的事件发射器实现。
///
/// 该实现基于 `indicatif::MultiProgress` 在终端显示进度条，
/// 并实现了 `EventEmitter` trait（见 `core::types`），以便库代码可以
/// 在发送/接收流程中透明地发出事件。
pub struct CliEventEmitter {
    mp: Arc<MultiProgress>,
    pb: Mutex<Option<ProgressBar>>,
    prefix: String,
}

impl CliEventEmitter {
    /// 创建一个新的 `CliEventEmitter`。
    ///
    /// `prefix` 用于在进度条前显示，例如 "[send]" 或 "[recv]"。
    pub fn new(prefix: &str) -> Self {
        Self {
            mp: Arc::new(MultiProgress::new()),
            pb: Mutex::new(None),
            prefix: prefix.to_string(),
        }
    }

    // 创建并返回进度条样式（内部使用）。
    fn make_progress_style() -> ProgressStyle {
        ProgressStyle::with_template("{prefix}{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {binary_bytes_per_sec}")
            .unwrap()
            .progress_chars("#>-")
    }
}

impl EventEmitter for CliEventEmitter {
    fn emit_event(&self, event_name: &str) -> Result<(), String> {
        match event_name {
            "transfer-started" | "receive-started" => {
                let mut guard = self.pb.lock().unwrap();
                if guard.is_none() {
                    let pb = self.mp.add(ProgressBar::new(0));
                    pb.set_style(Self::make_progress_style());
                    pb.enable_steady_tick(Duration::from_millis(250));
                    pb.set_prefix(format!("{} ", self.prefix));
                    *guard = Some(pb);
                }
                Ok(())
            }
            "transfer-completed" | "receive-completed" => {
                let mut guard = self.pb.lock().unwrap();
                if let Some(pb) = guard.take() {
                    pb.finish_and_clear();
                }
                Ok(())
            }
            "transfer-failed" | "receive-failed" => {
                let mut guard = self.pb.lock().unwrap();
                if let Some(pb) = guard.take() {
                    pb.abandon();
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// 处理带负载的事件；解析负载并更新进度条状态。
    fn emit_event_with_payload(&self, _event_name: &str, payload: &str) -> Result<(), String> {
        let parts: Vec<&str> = payload.split(':').collect();
        if parts.len() < 2 {
            return Err(format!("invalid payload: {}", payload));
        }
        let bytes: u64 = parts[0]
            .parse()
            .map_err(|e| format!("failed parse bytes: {}", e))?;
        let total: u64 = parts[1]
            .parse()
            .map_err(|e| format!("failed parse total: {}", e))?;
        let speed = if parts.len() > 2 {
            parts[2].parse::<i64>().unwrap_or(0)
        } else {
            0
        };

        let mut guard = self.pb.lock().unwrap();
        if guard.is_none() {
            let pb = self.mp.add(ProgressBar::new(total));
            pb.set_style(Self::make_progress_style());
            pb.set_prefix(format!("{} ", self.prefix));
            pb.enable_steady_tick(Duration::from_millis(250));
            pb.set_length(total);
            pb.set_position(bytes);
            *guard = Some(pb);
            return Ok(());
        }
        if let Some(pb) = guard.as_ref() {
            pb.set_length(total);
            pb.set_position(bytes);
            pb.set_message(format!("{}/s", humantime_bytes_per_sec(speed)));
        }
        Ok(())
    }
}

/// 将内部的速率整数（约定为 speed * 1000）格式化为人类可读的速率字符串。
fn humantime_bytes_per_sec(speed_milli: i64) -> String {
    let speed = (speed_milli as f64) / 1000.0;
    if speed <= 0.0 {
        return "0 B/s".to_string();
    }
    let units = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut val = speed;
    let mut idx = 0;
    while val >= 1024.0 && idx + 1 < units.len() {
        val /= 1024.0;
        idx += 1;
    }
    format!("{:.1} {}", val, units[idx])
}
