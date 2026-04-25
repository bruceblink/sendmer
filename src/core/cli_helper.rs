//! 进度与事件发射相关的工具与 CLI 辅助实现。
//!
//! 本模块包含用于向外部 `EventEmitter` 发射事件的便捷函数，
//! 以及一个命令行环境下的事件发射器实现 `CliEventEmitter`，
//! 用于在控制台显示文件传输进度条。

use crate::core::events::{EventEmitter, TransferEvent};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
    /// `prefix` 用于在进度条前显示，例如 "\[send\]" 或 "\[recv\]"。
    pub fn new(prefix: &str) -> Self {
        Self {
            mp: Arc::new(MultiProgress::new()),
            pb: Mutex::new(None),
            prefix: prefix.to_string(),
        }
    }

    // 创建并返回进度条样式（内部使用）。
    fn make_progress_style() -> ProgressStyle {
        #[allow(clippy::literal_string_with_formatting_args)]
        let template = "{prefix}{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {binary_bytes_per_sec}";
        ProgressStyle::with_template(template).map_or_else(
            |_| ProgressStyle::default_bar(),
            |style| style.progress_chars("#>-"),
        )
    }
}

impl EventEmitter for CliEventEmitter {
    fn emit(&self, event: &TransferEvent) {
        match event {
            TransferEvent::Started { .. } => {
                let mut guard = self.pb.lock().unwrap_or_else(|error| error.into_inner());
                if guard.is_none() {
                    let pb = self.mp.add(ProgressBar::new(0));
                    pb.set_style(Self::make_progress_style());
                    pb.enable_steady_tick(Duration::from_millis(250));
                    pb.set_prefix(format!("{} ", self.prefix));
                    *guard = Some(pb);
                }
            }

            TransferEvent::Progress {
                processed,
                total,
                speed,
                ..
            } => {
                let mut guard = self.pb.lock().unwrap_or_else(|error| error.into_inner());

                if guard.is_none() {
                    let pb = self.mp.add(ProgressBar::new(*total));
                    pb.set_style(Self::make_progress_style());
                    pb.enable_steady_tick(Duration::from_millis(250));
                    pb.set_prefix(format!("{} ", self.prefix));
                    pb.set_length(*total);
                    pb.set_position(*processed);
                    *guard = Some(pb);
                    return;
                }

                if let Some(pb) = guard.as_ref() {
                    pb.set_length(*total);
                    pb.set_position(*processed);
                    pb.set_message(human_bytes_per_sec(*speed));
                }
            }

            TransferEvent::Completed { .. } => {
                let value = self
                    .pb
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(pb) = value {
                    pb.finish_and_clear();
                }
            }

            TransferEvent::Failed { message, .. } => {
                let value = self
                    .pb
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(pb) = value {
                    pb.abandon();
                }
                eprintln!("Transfer failed: {message}");
            }
            TransferEvent::FileNames { .. } => {
                // skipping
            }
        }
    }
}

/// 将字节每秒速率格式化为人类可读的字符串。
fn human_bytes_per_sec(speed: f64) -> String {
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

#[cfg(test)]
mod tests {
    use super::human_bytes_per_sec;

    #[test]
    fn human_bytes_per_sec_formats_zero() {
        assert_eq!(human_bytes_per_sec(0.0), "0 B/s");
    }

    #[test]
    fn human_bytes_per_sec_formats_kilobytes_once() {
        assert_eq!(human_bytes_per_sec(2048.0), "2.0 KB/s");
    }
}
