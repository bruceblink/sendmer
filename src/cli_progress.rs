use std::sync::{Arc, Mutex};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::time::Duration;
use crate::core::types::EventEmitter;

pub struct CliEventEmitter {
    mp: Arc<MultiProgress>,
    pb: Mutex<Option<ProgressBar>>,
    // prefix can be used to distinguish send/receive
    prefix: String,
}

impl CliEventEmitter {
    pub fn new(prefix: &str) -> Self {
        Self {
            mp: Arc::new(MultiProgress::new()),
            pb: Mutex::new(None),
            prefix: prefix.to_string(),
        }
    }

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
                // create a progress bar if none
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
                    // drop by removing from multiprogress is handled implicitly
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

    fn emit_event_with_payload(&self, _event_name: &str, payload: &str) -> Result<(), String> {
        // payload format: "bytes:total:speed_int"
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
            // create if missing
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

fn humantime_bytes_per_sec(speed_milli: i64) -> String {
    // speed_milli is speed * 1000 per convention in the codebase
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
