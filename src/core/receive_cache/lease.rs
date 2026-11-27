use super::model::*;
use super::prune::*;
use super::snapshot::RecoveryJournal;
use crate::core::events::TransferSessionId;
use crate::core::options::ReceiveCacheOptions;
use crate::core::recovery::RecoveryTerminalState;
use anyhow::Context;
use fs4::{FileExt, TryLockError};
use iroh_blobs::HashAndFormat;
use iroh_blobs::api::remote::LocalInfo;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Holds the advisory lock for one content-addressed receive-cache entry.
///
/// Dropping the lease releases the operating-system lock. Call `preserve` or
/// `remove` after the iroh store shuts down so Windows does not retain open
/// database handles while the entry is being finalized.
pub struct ReceiveCacheLease {
    cache_root: PathBuf,
    entry_dir: PathBuf,
    lock_file: File,
    snapshot_ttl_seconds: u64,
    recovery: Mutex<Option<RecoveryJournal>>,
}

impl ReceiveCacheLease {
    /// Open or create one cache entry and acquire its non-blocking exclusive lock.
    pub fn open(
        options: &ReceiveCacheOptions,
        hash_and_format: HashAndFormat,
    ) -> anyhow::Result<Self> {
        options.validate()?;
        let cache_root = prepare_cache_root(&options.root_dir)?;
        match prune_cache_root(&cache_root) {
            Ok(report) if report.removed_entries > 0 => tracing::info!(
                removed_entries = report.removed_entries,
                "removed expired receive cache entries"
            ),
            Ok(_) => {}
            Err(error) => tracing::warn!(error = %error, "unable to prune receive cache"),
        }
        let root_lock = open_lock_file(&cache_root.join(CACHE_ROOT_LOCK_FILE))?;
        match FileExt::try_lock_shared(&root_lock) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!("receive cache maintenance is already in progress")
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("lock receive cache root");
            }
        }
        ensure_cache_capacity(&cache_root)?;
        let layout_dir = prepare_plain_directory(&cache_root.join(CACHE_LAYOUT_DIR))?.0;
        let canonical_layout = layout_dir
            .canonicalize()
            .context("canonicalize receive cache layout")?;
        anyhow::ensure!(
            canonical_layout.starts_with(&cache_root),
            "receive cache layout escapes its configured root"
        );

        let cache_key = cache_key(hash_and_format);
        let (entry_dir, entry_created) =
            prepare_plain_directory(&canonical_layout.join(&cache_key))?;
        let canonical_entry = entry_dir
            .canonicalize()
            .context("canonicalize receive cache entry")?;
        anyhow::ensure!(
            canonical_entry.starts_with(&canonical_layout),
            "receive cache entry escapes its layout directory"
        );

        let lock_file = open_lock_file(&canonical_entry.join(CACHE_LOCK_FILE))?;
        match FileExt::try_lock(&lock_file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!("receive cache entry is already in use")
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("lock receive cache entry");
            }
        }

        let snapshot_ttl_seconds = validate_or_create_manifest(
            &canonical_entry,
            &cache_key,
            options.ttl.as_secs(),
            entry_created,
        )?;
        touch_lock_file(&lock_file)?;
        FileExt::unlock(&root_lock).context("unlock receive cache root")?;

        Ok(Self {
            cache_root,
            entry_dir: canonical_entry,
            lock_file,
            snapshot_ttl_seconds,
            recovery: Mutex::new(None),
        })
    }

    pub fn entry_dir(&self) -> &Path {
        &self.entry_dir
    }

    /// Initialize a new event-session journal while this entry is exclusively locked.
    pub fn begin_recovery(&self, session_id: TransferSessionId) {
        let mut recovery = self
            .recovery
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *recovery = RecoveryJournal::open(&self.entry_dir, self.snapshot_ttl_seconds, session_id);
    }

    pub fn set_recovery_layout(&self, sizes: Arc<[u64]>) {
        if let Some(journal) = self
            .recovery
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
        {
            journal.set_layout(sizes);
        }
    }

    /// Invalidate the reuse proof before any new observation or barrier can fail.
    pub(crate) fn begin_checkpoint(&self) -> bool {
        let mut recovery = self
            .recovery
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        recovery.as_mut().is_some_and(|journal| {
            journal.invalidate_complete_checkpoint();
            true
        })
    }

    /// A fresh checkpoint is from this session and follows a successful store
    /// barrier, atomic journal write and heartbeat update.
    pub(crate) fn has_fresh_complete_checkpoint(&self) -> bool {
        let Ok(now) = unix_seconds() else {
            return false;
        };
        self.recovery
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|journal| journal.has_fresh_complete_checkpoint_at(now))
    }

    /// The journal is advisory: failed metadata writes cannot invalidate verified blobs.
    pub fn checkpoint_recovery(&self, local: &LocalInfo, processed: Option<u64>) {
        let mut recovery = self
            .recovery
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(journal) = recovery.as_mut()
            && journal
                .checkpoint(local, processed)
                .and_then(|written| {
                    if written {
                        touch_lock_file(&self.lock_file)
                    } else {
                        Ok(())
                    }
                })
                .is_err()
        {
            tracing::warn!("receive recovery checkpoint unavailable; retain verified blob data");
            *recovery = None;
        }
    }

    pub fn finish_recovery(&self, terminal: RecoveryTerminalState) {
        let mut recovery = self
            .recovery
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(journal) = recovery.as_mut()
            && journal.finish(terminal).is_err()
        {
            tracing::warn!("unable to persist receive recovery terminal");
            *recovery = None;
        }
    }

    /// 在传输进行中检查缓存容量和磁盘余量，防止预检查通过后继续无限增长。
    pub fn check_capacity(&self) -> anyhow::Result<()> {
        let root_lock = lock_root_shared(&self.cache_root)?;
        let result = ensure_cache_capacity(&self.cache_root);
        let unlock_result =
            FileExt::unlock(&root_lock).context("unlock receive cache root after capacity check");
        result.and(unlock_result)
    }

    /// Keep verified ranges for a later process and release the lease.
    pub fn preserve(self) -> anyhow::Result<()> {
        // 释放 entry 锁后再取得 root shared 锁，统一使用 root -> entry 的顺序。
        // prune 持有 root exclusive 后会等待 entry 锁，保留路径不能反向等待 root。
        FileExt::unlock(&self.lock_file).context("unlock receive cache entry before preserve")?;
        let root_lock = lock_root_shared(&self.cache_root)?;
        let entry_relocked = match FileExt::try_lock(&self.lock_file) {
            Ok(()) => true,
            Err(TryLockError::WouldBlock) => false,
            Err(TryLockError::Error(error)) => {
                let _ = FileExt::unlock(&root_lock);
                return Err(error).context("relock receive cache entry during preserve");
            }
        };
        if !entry_relocked {
            FileExt::unlock(&root_lock).context("unlock receive cache root after preserve")?;
            return Ok(());
        }

        let touch_result = touch_lock_file(&self.lock_file);
        let capacity_result = if touch_result.is_ok() {
            ensure_cache_capacity(&self.cache_root)
        } else {
            Ok(())
        };
        let unlock_entry_result =
            FileExt::unlock(&self.lock_file).context("unlock receive cache entry after preserve");
        let unlock_root_result =
            FileExt::unlock(&root_lock).context("unlock receive cache root after preserve");
        match (
            touch_result,
            capacity_result,
            unlock_entry_result,
            unlock_root_result,
        ) {
            (Err(error), _, _, _) => Err(error),
            (Ok(()), Err(error), _, _) => Err(error),
            (Ok(()), Ok(()), Err(error), _) => Err(error),
            (Ok(()), Ok(()), Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(()), Ok(()), Ok(())) => Ok(()),
        }
    }

    /// Release the lease before removing a completed cache entry.
    pub async fn remove(self) -> anyhow::Result<()> {
        let Self {
            cache_root,
            entry_dir,
            lock_file,
            ..
        } = self;
        // 先释放旧 entry 锁，再取得 root exclusive；否则新 receive 可能已持有
        // root shared 并等待同一个 entry 锁，导致删除路径和打开路径互相等待。
        FileExt::unlock(&lock_file).context("unlock completed receive cache entry")?;
        drop(lock_file);

        let Some(root_lock) = try_lock_root_exclusive(&cache_root)? else {
            // 维护任务正在运行时保留条目，成功接收不能因为清理竞争而失败。
            return Ok(());
        };
        let replacement_lock = match open_existing_lock_file(&entry_dir.join(CACHE_LOCK_FILE))? {
            Some(lock_file) => lock_file,
            None => {
                FileExt::unlock(&root_lock).context("unlock receive cache root after remove")?;
                return Ok(());
            }
        };
        match FileExt::try_lock(&replacement_lock) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                // 新 receive 已经接管了该条目。root exclusive 保证它不能在此后
                // 重新进入，因此这里必须保留新条目而不是删除其目录。
                FileExt::unlock(&root_lock).context("unlock receive cache root after remove")?;
                return Ok(());
            }
            Err(TryLockError::Error(error)) => {
                FileExt::unlock(&root_lock).context("unlock receive cache root after remove")?;
                return Err(error).context("lock completed receive cache entry for removal");
            }
        }

        let unlock_entry_result =
            FileExt::unlock(&replacement_lock).context("unlock completed receive cache entry");
        drop(replacement_lock);
        if let Err(error) = unlock_entry_result {
            let _ = FileExt::unlock(&root_lock);
            return Err(error);
        }

        let remove_result = match tokio::fs::remove_dir_all(&entry_dir).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove completed receive cache entry"),
        };
        let unlock_root_result =
            FileExt::unlock(&root_lock).context("unlock receive cache root after remove");
        match (remove_result, unlock_root_result) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}
