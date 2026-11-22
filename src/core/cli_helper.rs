//! 进度与事件发射相关的工具与 CLI 辅助实现。
//!
//! 本模块包含用于向外部 `EventEmitter` 发射事件的便捷函数，
//! 以及一个命令行环境下的事件发射器实现 `CliEventEmitter`，
//! 用于在控制台显示文件传输进度条。

use crate::core::events::{
    EventEmitter, TransferEvent, TransferEventData, TransferEventStreamError,
    TransferEventStreamValidator, TransferSessionId,
};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
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
    json_lines: bool,
    json_validator: Option<Mutex<HashMap<TransferSessionId, TransferEventStreamValidator>>>,
    json_output_closed: AtomicBool,
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
            json_lines: false,
            json_validator: None,
            json_output_closed: AtomicBool::new(false),
        }
    }

    /// Create an emitter that writes one stable JSON event per stdout line.
    pub fn json_lines() -> Self {
        Self {
            mp: Arc::new(MultiProgress::new()),
            pb: Mutex::new(None),
            prefix: String::new(),
            json_lines: true,
            json_validator: Some(Mutex::new(HashMap::new())),
            json_output_closed: AtomicBool::new(false),
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

/// 校验可复用 CLI emitter 中的多个并行或连续传输会话。
fn validate_json_event(
    validators: &mut HashMap<TransferSessionId, TransferEventStreamValidator>,
    event: &TransferEvent,
) -> Result<(), TransferEventStreamError> {
    let session_id = event.session_id.clone();
    let validator = validators.entry(session_id.clone()).or_default();
    validator.accept(event)?;
    if validator.is_terminal() {
        validators.remove(&session_id);
    }
    Ok(())
}

/// 写入一行 JSON 并立即刷新，便于管道消费者及时取得传输状态。
fn write_json_line<W: io::Write>(writer: &mut W, json: &str) -> io::Result<()> {
    writeln!(writer, "{json}")?;
    writer.flush()
}

impl EventEmitter for CliEventEmitter {
    fn emit(&self, event: &TransferEvent) {
        if self.json_lines {
            if self.json_output_closed.load(Ordering::Acquire) {
                return;
            }
            if let Some(validator) = &self.json_validator {
                let validation = {
                    let mut validators =
                        validator.lock().unwrap_or_else(|error| error.into_inner());
                    validate_json_event(&mut validators, event)
                };
                if let Err(error) = validation {
                    eprintln!("rejected transfer event: {error}");
                    return;
                }
            }
            match serde_json::to_string(event) {
                Ok(json) => {
                    let stdout = std::io::stdout();
                    let mut stdout = stdout.lock();
                    if let Err(error) = write_json_line(&mut stdout, &json) {
                        self.json_output_closed.store(true, Ordering::Release);
                        if error.kind() != io::ErrorKind::BrokenPipe {
                            eprintln!("failed to write transfer event: {error}");
                        }
                    }
                }
                Err(error) => {
                    self.json_output_closed.store(true, Ordering::Release);
                    eprintln!("failed to serialize transfer event: {error}");
                }
            }
            return;
        }

        match &event.event {
            TransferEventData::Started => {
                let mut guard = self.pb.lock().unwrap_or_else(|error| error.into_inner());
                if guard.is_none() {
                    let pb = self.mp.add(ProgressBar::new(0));
                    pb.set_style(Self::make_progress_style());
                    pb.enable_steady_tick(Duration::from_millis(250));
                    pb.set_prefix(format!("{} ", self.prefix));
                    *guard = Some(pb);
                }
            }

            TransferEventData::Progress {
                processed,
                total,
                speed_bytes_per_sec,
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
                    pb.set_message(human_bytes_per_sec(*speed_bytes_per_sec));
                }
            }

            TransferEventData::Completed => {
                let value = self
                    .pb
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(pb) = value {
                    pb.finish_and_clear();
                }
            }

            TransferEventData::Failed { error } => {
                let value = self
                    .pb
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(pb) = value {
                    pb.abandon();
                }
                eprintln!("Transfer failed: {}", error.message);
            }
            TransferEventData::Cancelled => {
                let value = self
                    .pb
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .take();
                if let Some(pb) = value {
                    pb.abandon();
                }
                eprintln!("Transfer cancelled");
            }
            TransferEventData::FileNames { .. } => {
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
    use super::{human_bytes_per_sec, validate_json_event, write_json_line};
    use crate::core::events::{Role, TransferEventData, TransferEventEnvelope, TransferSessionId};
    use std::collections::HashMap;
    use std::io;
    use std::str::FromStr;

    #[test]
    fn human_bytes_per_sec_formats_zero() {
        assert_eq!(human_bytes_per_sec(0.0), "0 B/s");
    }

    #[test]
    fn human_bytes_per_sec_formats_kilobytes_once() {
        assert_eq!(human_bytes_per_sec(2048.0), "2.0 KB/s");
    }

    #[test]
    fn json_validator_supports_multiple_sessions() {
        let first =
            TransferSessionId::from_str("0123456789abcdef0123456789abcdef").expect("first session");
        let second = TransferSessionId::from_str("abcdefabcdefabcdefabcdefabcdefab")
            .expect("second session");
        let mut validators = HashMap::new();
        for session in [first, second] {
            validate_json_event(
                &mut validators,
                &TransferEventEnvelope::new(
                    session.clone(),
                    1,
                    1,
                    Role::Receiver,
                    crate::core::events::TransferPhase::Preparing,
                    TransferEventData::Started,
                ),
            )
            .expect("started event");
            validate_json_event(
                &mut validators,
                &TransferEventEnvelope::new(
                    session,
                    2,
                    2,
                    Role::Receiver,
                    crate::core::events::TransferPhase::Finalizing,
                    TransferEventData::Completed,
                ),
            )
            .expect("completed event");
        }
        assert!(validators.is_empty());
    }

    #[test]
    fn json_writer_reports_broken_pipe_without_panicking() {
        let mut writer = BrokenPipeWriter;
        let error = write_json_line(&mut writer, "{}").expect_err("broken pipe");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    struct BrokenPipeWriter;

    impl io::Write for BrokenPipeWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::BrokenPipe))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
