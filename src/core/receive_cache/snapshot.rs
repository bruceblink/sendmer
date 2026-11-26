//! Bounded, advisory recovery metadata owned by the exclusive cache lease.
//! The iroh store remains the only authority for missing ranges and export.

use super::prune::unix_seconds;
use crate::core::events::{Role, TransferSessionId};
use crate::core::recovery::{
    RECOVERY_SNAPSHOT_SCHEMA_VERSION, RecoverySnapshot, RecoveryTerminalState,
};
use anyhow::Context;
use iroh_blobs::api::remote::LocalInfo;
use iroh_blobs::protocol::GetRequest;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SNAPSHOT_FILE: &str = "recovery.json";
const SNAPSHOT_TEMP: &str = ".recovery.tmp";
const MAX_SNAPSHOT_BYTES: u64 = 1024 * 1024;
const MAX_RANGES: usize = 8192;
const CHUNK_BYTES: u64 = 1024;

#[derive(Debug, Serialize, Deserialize)]
struct RecoveryRecord {
    cache_key: String,
    updated_at_unix_seconds: u64,
    transfer_total: u64,
    snapshot: RecoverySnapshot,
}

impl RecoveryRecord {
    fn valid_at(&self, key: &str, now: u64, ttl: u64) -> bool {
        let snapshot = &self.snapshot;
        self.cache_key == key
            && snapshot.schema_version == RECOVERY_SNAPSHOT_SCHEMA_VERSION
            && snapshot.role == Role::Receiver
            && self.updated_at_unix_seconds <= now
            && now - self.updated_at_unix_seconds < ttl
            && snapshot.processed_bytes <= self.transfer_total
            && snapshot.verified_ranges.len() <= MAX_RANGES
            && snapshot
                .verified_ranges
                .iter()
                .all(|range| range.end <= self.transfer_total && !range.is_empty())
            && snapshot
                .verified_ranges
                .windows(2)
                .all(|pair| pair[0].end < pair[1].start)
            && snapshot.verified_bytes() <= snapshot.processed_bytes
    }

    fn resumable_at(&self, key: &str, now: u64, ttl: u64) -> bool {
        self.valid_at(key, now, ttl) && self.snapshot.can_resume()
    }
}

pub(super) struct RecoveryJournal {
    entry_dir: PathBuf,
    record: RecoveryRecord,
    previous: Option<RecoveryRecord>,
    sizes: Option<Arc<[u64]>>,
}

impl RecoveryJournal {
    pub(super) fn open(entry_dir: &Path, ttl: u64, session_id: TransferSessionId) -> Option<Self> {
        if std::env::var_os("SENDMER_DISABLE_RECOVERY_SNAPSHOT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            return None;
        }
        Self::open_enabled(entry_dir, ttl, session_id)
            .inspect_err(|_| {
                tracing::warn!("receive recovery metadata unavailable; using verified blob store")
            })
            .ok()
    }

    fn open_enabled(
        entry_dir: &Path,
        ttl: u64,
        session_id: TransferSessionId,
    ) -> anyhow::Result<Self> {
        let key = entry_dir
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid cache key")?;
        let now = unix_seconds()?;
        let previous = read_record(&entry_dir.join(SNAPSHOT_FILE))?
            .filter(|record| record.resumable_at(key, now, ttl));
        Ok(Self {
            entry_dir: entry_dir.to_path_buf(),
            record: RecoveryRecord {
                cache_key: key.to_owned(),
                updated_at_unix_seconds: now,
                transfer_total: 0,
                snapshot: RecoverySnapshot::new(Role::Receiver, session_id),
            },
            previous,
            sizes: None,
        })
    }

    pub(super) fn set_layout(&mut self, sizes: Arc<[u64]>) {
        self.sizes = Some(sizes);
    }

    pub(super) fn checkpoint(
        &mut self,
        local: &LocalInfo,
        processed: Option<u64>,
    ) -> anyhow::Result<bool> {
        // An incomplete hash sequence has no reliable flattened coordinate system
        // until the sender's verified metadata sizes have been obtained.
        let (total, ranges) = if let Some(sizes) = &self.sizes {
            ranges_from_missing(&local.missing(), sizes)?
        } else if local.is_complete() {
            let total = local.local_bytes();
            let ranges = if total == 0 {
                Vec::new()
            } else {
                std::iter::once(0..total).collect()
            };
            (total, ranges)
        } else {
            return Ok(false);
        };
        anyhow::ensure!(
            ranges.iter().map(|r| r.end - r.start).sum::<u64>() <= local.local_bytes(),
            "recovery ranges exceed verified local bytes"
        );
        self.record.transfer_total = total;
        // Reuse a prior summary only after comparison to the present store. Even
        // a structurally valid snapshot can be stale after a process crash.
        if let Some(previous) = self.previous.take()
            && previous.transfer_total == total
            && ranges_cover(&ranges, &previous.snapshot.verified_ranges)
        {
            tracing::debug!(
                verified_bytes = previous.snapshot.verified_bytes(),
                "revalidated previous receive recovery snapshot"
            );
        }
        self.record.snapshot.verified_ranges = ranges;
        // Progress is telemetry, not range evidence. Start a fresh session ID
        // and never import the previous session's processed count or terminal.
        let verified = self.record.snapshot.verified_bytes();
        self.record
            .snapshot
            .record_progress(processed.unwrap_or(verified).max(verified).min(total));
        self.persist()?;
        Ok(true)
    }

    pub(super) fn finish(&mut self, terminal: RecoveryTerminalState) -> anyhow::Result<()> {
        self.record.snapshot.record_terminal_state(terminal);
        self.persist()
    }

    fn persist(&mut self) -> anyhow::Result<()> {
        self.record.updated_at_unix_seconds = unix_seconds()?;
        atomic_write(&self.entry_dir, &self.record)
    }
}

/// Flatten the verified complement of missing BAO chunks into byte offsets:
/// root hash sequence, then every child (including collection metadata), in
/// hash-sequence order. Duplicate hashes occupy their own logical positions.
fn ranges_from_missing(
    missing: &GetRequest,
    sizes: &[u64],
) -> anyhow::Result<(u64, Vec<Range<u64>>)> {
    let root_size = u64::try_from(sizes.len())?
        .checked_mul(32)
        .context("recovery root size overflow")?;
    let mut base = 0u64;
    let mut result = Vec::new();
    for (index, size) in std::iter::once(root_size)
        .chain(sizes.iter().copied())
        .enumerate()
    {
        let end = base
            .checked_add(size)
            .context("recovery transfer size overflow")?;
        let mut start = 0u64;
        let mut present = true;
        for boundary in missing.ranges[u64::try_from(index)?].boundaries() {
            let offset = boundary.0.saturating_mul(CHUNK_BYTES).min(size);
            if present {
                push_range(&mut result, (base + start)..(base + offset))?;
            }
            start = offset;
            present = !present;
        }
        if present {
            push_range(&mut result, (base + start)..end)?;
        }
        base = end;
    }
    Ok((base, result))
}

/// Accept older ranges only if every byte is still represented by the live store.
fn ranges_cover(live: &[Range<u64>], previous: &[Range<u64>]) -> bool {
    let mut live = live.iter().peekable();
    for range in previous {
        while live.peek().is_some_and(|item| item.end <= range.start) {
            live.next();
        }
        if !live
            .peek()
            .is_some_and(|item| item.start <= range.start && item.end >= range.end)
        {
            return false;
        }
    }
    true
}

fn push_range(ranges: &mut Vec<Range<u64>>, range: Range<u64>) -> anyhow::Result<()> {
    if range.start >= range.end {
        return Ok(());
    }
    if let Some(last) = ranges.last_mut()
        && last.end == range.start
    {
        last.end = range.end;
    } else {
        anyhow::ensure!(ranges.len() < MAX_RANGES, "too many recovery ranges");
        ranges.push(range);
    }
    Ok(())
}

/// Read at most the format's size limit. Missing/corrupt supported metadata
/// falls back to the store. Unknown versions and non-plain files are preserved.
fn read_record(path: &Path) -> anyhow::Result<Option<RecoveryRecord>> {
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(metadata) => anyhow::ensure!(
            metadata.is_file()
                && !metadata.file_type().is_symlink()
                && metadata.len() <= MAX_SNAPSHOT_BYTES,
            "unsafe or oversized recovery record"
        ),
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_SNAPSHOT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_SNAPSHOT_BYTES,
        "recovery record too large"
    );
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Ok(None);
    };
    if let Some(version) = value
        .get("snapshot")
        .and_then(|s| s.get("schema_version"))
        .and_then(serde_json::Value::as_u64)
    {
        anyhow::ensure!(
            version == u64::from(RECOVERY_SNAPSHOT_SCHEMA_VERSION),
            "unsupported recovery snapshot schema"
        );
    }
    Ok(serde_json::from_value(value).ok())
}

fn atomic_write(entry_dir: &Path, record: &RecoveryRecord) -> anyhow::Result<()> {
    let target = entry_dir.join(SNAPSHOT_FILE);
    let temp = entry_dir.join(SNAPSHOT_TEMP);
    // The exclusive lease serializes cooperating writers and makes an abandoned
    // same-directory temp safe to remove. Never follow a symlink or special file.
    check_plain_file(&target)?;
    if check_plain_file(&temp)? {
        std::fs::remove_file(&temp)?;
    }
    let bytes = serde_json::to_vec(record)?;
    anyhow::ensure!(
        (bytes.len() as u64) < MAX_SNAPSHOT_BYTES,
        "recovery record too large"
    );
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)?;
    let result = (|| {
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = result.and_then(|()| std::fs::rename(&temp, &target)) {
        let _ = std::fs::remove_file(&temp);
        return Err(error.into());
    }
    // Flush the directory entry on Unix. Windows uses same-directory rename
    // with file flush; no cross-platform power-loss durability claim.
    #[cfg(unix)]
    File::open(entry_dir)?.sync_all()?;
    Ok(())
}

fn check_plain_file(path: &Path) -> anyhow::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "unsafe recovery path"
            );
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests;
