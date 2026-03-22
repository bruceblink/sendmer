use std::time::{Duration, Instant};

pub struct ProgressTracker {
    start: Instant,
    last_emit: Instant,
    current: u64,
    total: u64,
}

pub struct ProgressSnapshot {
    pub current: u64,
    pub total: u64,
    pub speed: f64,
}

impl ProgressTracker {
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            start: now,
            last_emit: now,
            current: 0,
            total: 0,
        }
    }

    pub fn set_total(&mut self, total: u64) {
        self.total = total;
    }

    pub fn update(&mut self, current: u64) -> Option<ProgressSnapshot> {
        self.current = current;

        if self.last_emit.elapsed() < Duration::from_millis(200) {
            return None;
        }

        self.last_emit = Instant::now();

        let elapsed = self.start.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 {
            self.current as f64 / elapsed
        } else {
            0.0
        };

        Some(ProgressSnapshot {
            current: self.current,
            total: self.total,
            speed,
        })
    }
}