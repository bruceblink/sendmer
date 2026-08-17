//! Persistent receive-cache ownership, metadata, and cross-process locking.

use crate::core::options::ReceiveCacheOptions;
use anyhow::Context;
use fs4::{FileExt, TryLockError};
use iroh_blobs::HashAndFormat;
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_LAYOUT_DIR: &str = "v1";
const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_MANIFEST_FILE: &str = "manifest.json";
const CACHE_LOCK_FILE: &str = ".lock";
const CACHE_ROOT_LOCK_FILE: &str = ".prune.lock";
const MANIFEST_TEMP_PREFIX: &str = ".manifest-";

#[derive(Debug, Deserialize, Serialize)]
struct ReceiveCacheManifest {
    schema_version: u32,
    cache_key: String,
    created_at_unix_seconds: u64,
    ttl_seconds: u64,
}

/// Summary returned by receive-cache maintenance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveCachePruneReport {
    pub removed_entries: u64,
    pub retained_entries: u64,
    pub active_entries: u64,
    pub unknown_entries: u64,
}

/// Remove expired receive-cache entries without touching active or unknown data.
///
/// Each entry uses the TTL recorded in its versioned manifest. A missing root
/// is treated as an empty cache; symlinks, future schemas, and malformed entry
/// names are preserved rather than guessed at.
pub async fn prune_receive_cache(
    root_dir: impl AsRef<Path>,
) -> anyhow::Result<ReceiveCachePruneReport> {
    let root_dir = root_dir.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || {
        match std::fs::symlink_metadata(&root_dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ReceiveCachePruneReport::default());
            }
            Err(error) => return Err(error).context("inspect receive cache root"),
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "receive cache root must not be a symbolic link"
                );
                anyhow::ensure!(metadata.is_dir(), "receive cache root must be a directory");
            }
        }
        let canonical_root = root_dir
            .canonicalize()
            .context("canonicalize receive cache root")?;
        prune_cache_root(&canonical_root)
    })
    .await
    .context("join receive cache prune task")?
}

/// Holds the advisory lock for one content-addressed receive-cache entry.
///
/// Dropping the lease releases the operating-system lock. Call `preserve` or
/// `remove` after the iroh store shuts down so Windows does not retain open
/// database handles while the entry is being finalized.
pub(super) struct ReceiveCacheLease {
    entry_dir: PathBuf,
    lock_file: File,
}

impl ReceiveCacheLease {
    /// Open or create one cache entry and acquire its non-blocking exclusive lock.
    pub(super) fn open(
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

        let mut lock_file = open_lock_file(&canonical_entry.join(CACHE_LOCK_FILE))?;
        match FileExt::try_lock(&lock_file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                anyhow::bail!("receive cache entry is already in use")
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("lock receive cache entry");
            }
        }

        validate_or_create_manifest(
            &canonical_entry,
            &cache_key,
            options.ttl.as_secs(),
            entry_created,
        )?;
        touch_lock_file(&mut lock_file)?;
        FileExt::unlock(&root_lock).context("unlock receive cache root")?;

        Ok(Self {
            entry_dir: canonical_entry,
            lock_file,
        })
    }

    pub(super) fn entry_dir(&self) -> &Path {
        &self.entry_dir
    }

    /// Keep verified ranges for a later process and release the lease.
    pub(super) fn preserve(mut self) -> anyhow::Result<()> {
        let touch_result = touch_lock_file(&mut self.lock_file);
        let unlock_result = FileExt::unlock(&self.lock_file).context("unlock receive cache entry");
        touch_result.and(unlock_result)
    }

    /// Release the lease before removing a completed cache entry.
    pub(super) async fn remove(self) -> anyhow::Result<()> {
        let Self {
            entry_dir,
            lock_file,
        } = self;
        FileExt::unlock(&lock_file).context("unlock completed receive cache entry")?;
        drop(lock_file);

        match tokio::fs::remove_dir_all(&entry_dir).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove completed receive cache entry"),
        }
    }
}

fn prune_cache_root(cache_root: &Path) -> anyhow::Result<ReceiveCachePruneReport> {
    let layout_dir = cache_root.join(CACHE_LAYOUT_DIR);
    match std::fs::symlink_metadata(&layout_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReceiveCachePruneReport::default());
        }
        Err(error) => return Err(error).context("inspect receive cache layout"),
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink() && metadata.is_dir(),
                "receive cache layout must be a plain directory"
            );
        }
    }

    let root_lock = open_lock_file(&cache_root.join(CACHE_ROOT_LOCK_FILE))?;
    match FileExt::try_lock(&root_lock) {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            anyhow::bail!("receive cache maintenance is already in progress")
        }
        Err(TryLockError::Error(error)) => {
            return Err(error).context("lock receive cache root for pruning");
        }
    }

    let scan_result = prune_cache_entries(&layout_dir);
    let unlock_result =
        FileExt::unlock(&root_lock).context("unlock receive cache root after prune");
    match (scan_result, unlock_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(report), Ok(())) => Ok(report),
    }
}

fn prune_cache_entries(layout_dir: &Path) -> anyhow::Result<ReceiveCachePruneReport> {
    let mut report = ReceiveCachePruneReport::default();
    let now = unix_seconds()?;
    for item in std::fs::read_dir(layout_dir).context("scan receive cache layout")? {
        let item = item.context("read receive cache entry")?;
        let entry_dir = item.path();
        let Some(entry_name) = item.file_name().to_str().map(ToOwned::to_owned) else {
            report.unknown_entries += 1;
            continue;
        };
        let metadata = item
            .file_type()
            .context("inspect receive cache entry type")?;
        if !metadata.is_dir() || !is_cache_key(&entry_name) {
            report.unknown_entries += 1;
            continue;
        }
        let Some(ttl_seconds) = read_prunable_manifest(&entry_dir, &entry_name)? else {
            report.unknown_entries += 1;
            continue;
        };
        let Some(mut lock_file) = open_existing_lock_file(&entry_dir.join(CACHE_LOCK_FILE))? else {
            report.unknown_entries += 1;
            continue;
        };
        match FileExt::try_lock(&lock_file) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                report.active_entries += 1;
                continue;
            }
            Err(TryLockError::Error(error)) => {
                return Err(error).context("lock expired receive cache entry");
            }
        }

        let Some(last_used) = read_lock_heartbeat(&mut lock_file)? else {
            FileExt::unlock(&lock_file).context("unlock unknown receive cache entry")?;
            report.unknown_entries += 1;
            continue;
        };
        if now.saturating_sub(last_used) < ttl_seconds {
            FileExt::unlock(&lock_file).context("unlock retained receive cache entry")?;
            report.retained_entries += 1;
            continue;
        }

        FileExt::unlock(&lock_file).context("unlock expired receive cache entry")?;
        drop(lock_file);
        std::fs::remove_dir_all(&entry_dir).context("remove expired receive cache entry")?;
        report.removed_entries += 1;
    }
    Ok(report)
}

/// `None` means the entry is malformed or owned by a future schema and must be kept.
fn read_prunable_manifest(entry_dir: &Path, expected_key: &str) -> anyhow::Result<Option<u64>> {
    let manifest: ReceiveCacheManifest = match std::fs::read(entry_dir.join(CACHE_MANIFEST_FILE)) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(manifest) => manifest,
            Err(_) => return Ok(None),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read receive cache manifest during prune"),
    };
    if manifest.schema_version != CACHE_SCHEMA_VERSION
        || manifest.cache_key != expected_key
        || manifest.ttl_seconds == 0
    {
        return Ok(None);
    }
    Ok(Some(manifest.ttl_seconds))
}

fn open_existing_lock_file(path: &Path) -> anyhow::Result<Option<File>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Ok(None);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect receive cache heartbeat"),
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("open receive cache heartbeat")
        .map(Some)
}

/// Read through the same handle that owns the lock; Windows rejects a second
/// handle reading a byte range protected by `LockFileEx`.
fn read_lock_heartbeat(file: &mut File) -> anyhow::Result<Option<u64>> {
    let mut heartbeat = String::new();
    file.seek(SeekFrom::Start(0))
        .context("seek receive cache heartbeat")?;
    file.read_to_string(&mut heartbeat)
        .context("read receive cache heartbeat")?;
    Ok(heartbeat.trim().parse::<u64>().ok())
}

fn is_cache_key(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() == 66
        && matches!(bytes[0], b'0' | b'1')
        && bytes[1] == b'-'
        && bytes[2..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// Create the configured root, then reject a final symlink or non-directory.
fn prepare_cache_root(root: &Path) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(root).context("create receive cache root")?;
    let metadata = std::fs::symlink_metadata(root).context("inspect receive cache root")?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink(),
        "receive cache root must not be a symbolic link"
    );
    anyhow::ensure!(metadata.is_dir(), "receive cache root must be a directory");
    root.canonicalize()
        .context("canonicalize receive cache root")
}

/// Return a plain directory and whether this call created it.
fn prepare_plain_directory(path: &Path) -> anyhow::Result<(PathBuf, bool)> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "receive cache directory must not be a symbolic link"
            );
            anyhow::ensure!(metadata.is_dir(), "receive cache path must be a directory");
            Ok((path.to_path_buf(), false))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(path) {
                Ok(()) => Ok((path.to_path_buf(), true)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    prepare_plain_directory(path)
                }
                Err(error) => Err(error).context("create receive cache directory"),
            }
        }
        Err(error) => Err(error).context("inspect receive cache directory"),
    }
}

fn open_lock_file(path: &Path) -> anyhow::Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "receive cache lock must not be a symbolic link"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect receive cache lock"),
    }

    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .context("open receive cache lock")
}

/// Write a heartbeat timestamp without replacing the inode used for locking.
fn touch_lock_file(file: &mut File) -> anyhow::Result<()> {
    let now = unix_seconds()?;
    file.set_len(0).context("truncate receive cache lock")?;
    file.seek(SeekFrom::Start(0))
        .context("seek receive cache lock")?;
    writeln!(file, "{now}").context("refresh receive cache lock")?;
    file.sync_data().context("sync receive cache lock")
}

fn validate_or_create_manifest(
    entry_dir: &Path,
    expected_key: &str,
    ttl_seconds: u64,
    entry_created: bool,
) -> anyhow::Result<()> {
    let manifest_path = entry_dir.join(CACHE_MANIFEST_FILE);
    match std::fs::read(&manifest_path) {
        Ok(bytes) => {
            let manifest: ReceiveCacheManifest =
                serde_json::from_slice(&bytes).context("parse receive cache manifest")?;
            anyhow::ensure!(
                manifest.schema_version == CACHE_SCHEMA_VERSION,
                "unsupported receive cache schema version {}",
                manifest.schema_version
            );
            anyhow::ensure!(
                manifest.cache_key == expected_key,
                "receive cache manifest key does not match its directory"
            );
            anyhow::ensure!(
                manifest.ttl_seconds > 0,
                "receive cache manifest TTL must be greater than zero"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::ensure!(
                entry_created || cache_entry_has_only_control_files(entry_dir)?,
                "receive cache manifest is missing from an existing data entry"
            );
            remove_manifest_temps(entry_dir)?;
            let manifest = ReceiveCacheManifest {
                schema_version: CACHE_SCHEMA_VERSION,
                cache_key: expected_key.to_owned(),
                created_at_unix_seconds: unix_seconds()?,
                ttl_seconds,
            };
            write_new_manifest(&manifest_path, &manifest)
        }
        Err(error) => Err(error).context("read receive cache manifest"),
    }
}

fn cache_entry_has_only_control_files(entry_dir: &Path) -> anyhow::Result<bool> {
    for item in std::fs::read_dir(entry_dir).context("inspect receive cache entry")? {
        let item = item.context("read receive cache entry")?;
        let name = item.file_name();
        let name = name.to_string_lossy();
        if name != CACHE_LOCK_FILE && !name.starts_with(MANIFEST_TEMP_PREFIX) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn remove_manifest_temps(entry_dir: &Path) -> anyhow::Result<()> {
    for item in std::fs::read_dir(entry_dir).context("inspect receive cache manifest temps")? {
        let item = item.context("read receive cache manifest temp")?;
        if item
            .file_name()
            .to_string_lossy()
            .starts_with(MANIFEST_TEMP_PREFIX)
        {
            std::fs::remove_file(item.path()).context("remove receive cache manifest temp")?;
        }
    }
    Ok(())
}

/// Create the manifest through a same-directory temporary file so a crash
/// cannot expose partially serialized JSON as the committed metadata.
fn write_new_manifest(path: &Path, manifest: &ReceiveCacheManifest) -> anyhow::Result<()> {
    let suffix = rand::rng().random::<u64>();
    let temp_path = path.with_file_name(format!("{MANIFEST_TEMP_PREFIX}{suffix:016x}.tmp"));
    let bytes = serde_json::to_vec_pretty(manifest).context("serialize receive cache manifest")?;
    let mut temp = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp_path)
        .context("create receive cache manifest temp")?;
    temp.write_all(&bytes)
        .context("write receive cache manifest temp")?;
    temp.write_all(b"\n")
        .context("finish receive cache manifest temp")?;
    temp.sync_all()
        .context("sync receive cache manifest temp")?;
    drop(temp);
    std::fs::rename(&temp_path, path).context("commit receive cache manifest")
}

fn cache_key(hash_and_format: HashAndFormat) -> String {
    format!(
        "{}-{}",
        u64::from(hash_and_format.format),
        hash_and_format.hash.to_hex()
    )
}

fn unix_seconds() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")
        .map(|duration| duration.as_secs())
}

#[cfg(test)]
mod tests {
    use super::{
        CACHE_MANIFEST_FILE, CACHE_SCHEMA_VERSION, ReceiveCacheLease, ReceiveCacheManifest,
        cache_key, prune_receive_cache,
    };
    use crate::core::options::ReceiveCacheOptions;
    use iroh_blobs::{BlobFormat, Hash, HashAndFormat};

    fn hash_and_format(format: BlobFormat) -> HashAndFormat {
        HashAndFormat {
            hash: Hash::new(b"persistent receive cache test"),
            format,
        }
    }

    #[test]
    fn cache_key_is_content_addressed_and_format_specific() {
        let raw = cache_key(hash_and_format(BlobFormat::Raw));
        let collection = cache_key(hash_and_format(BlobFormat::HashSeq));
        assert_ne!(raw, collection);
        assert!(raw.starts_with("0-"));
        assert!(collection.starts_with("1-"));
    }

    #[test]
    fn second_lease_for_same_entry_is_rejected() {
        let root = tempfile::tempdir().expect("cache root");
        let options = ReceiveCacheOptions::new(root.path());
        let first = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("first lease");

        let error = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .err()
            .expect("second lease must fail");
        assert!(error.to_string().contains("already in use"));

        first.preserve().expect("release first lease");
        ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("lease after release")
            .preserve()
            .expect("release reopened lease");
    }

    #[test]
    fn preserved_entry_reopens_with_same_path() {
        let root = tempfile::tempdir().expect("cache root");
        let options = ReceiveCacheOptions::new(root.path());
        let first = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("first lease");
        let path = first.entry_dir().to_path_buf();
        first.preserve().expect("preserve cache entry");

        let second = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("reopen cache entry");
        assert_eq!(second.entry_dir(), path);
        second.preserve().expect("release cache entry");
    }

    #[tokio::test]
    async fn completed_entry_is_removed_after_unlock() {
        let root = tempfile::tempdir().expect("cache root");
        let options = ReceiveCacheOptions::new(root.path());
        let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("cache lease");
        let path = lease.entry_dir().to_path_buf();

        lease.remove().await.expect("remove completed entry");
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn prune_removes_expired_unlocked_entry() {
        let root = tempfile::tempdir().expect("cache root");
        let options =
            ReceiveCacheOptions::new(root.path()).with_ttl(std::time::Duration::from_secs(1));
        let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("cache lease");
        let entry = lease.entry_dir().to_path_buf();
        lease.preserve().expect("release cache entry");
        std::fs::write(entry.join(".lock"), b"0\n").expect("expire cache heartbeat");
        assert_eq!(
            std::fs::read_to_string(entry.join(".lock")).expect("read expired heartbeat"),
            "0\n"
        );

        let report = prune_receive_cache(root.path()).await.expect("prune cache");

        assert_eq!(report.removed_entries, 1);
        assert_eq!(report.active_entries, 0);
        assert!(!entry.exists());
    }

    #[tokio::test]
    async fn prune_skips_active_entry() {
        let root = tempfile::tempdir().expect("cache root");
        let options = ReceiveCacheOptions::new(root.path());
        let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("cache lease");
        let entry = lease.entry_dir().to_path_buf();

        let report = prune_receive_cache(root.path()).await.expect("prune cache");

        assert_eq!(report.removed_entries, 0);
        assert_eq!(report.active_entries, 1);
        assert!(entry.exists());
        lease.preserve().expect("release cache entry");
    }

    #[tokio::test]
    async fn prune_preserves_unknown_and_future_entries() {
        let root = tempfile::tempdir().expect("cache root");
        let options = ReceiveCacheOptions::new(root.path());
        let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("cache lease");
        let entry = lease.entry_dir().to_path_buf();
        lease.preserve().expect("release cache entry");

        let manifest_path = entry.join(CACHE_MANIFEST_FILE);
        let mut manifest: ReceiveCacheManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
                .expect("parse manifest");
        manifest.schema_version = CACHE_SCHEMA_VERSION + 1;
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
        )
        .expect("write future manifest");
        std::fs::write(entry.join(".lock"), b"0\n").expect("expire cache heartbeat");
        std::fs::create_dir_all(root.path().join("v1").join("not-an-entry"))
            .expect("unknown entry");

        let report = prune_receive_cache(root.path()).await.expect("prune cache");

        assert_eq!(report.removed_entries, 0);
        assert_eq!(report.unknown_entries, 2);
        assert!(entry.exists());
    }

    #[tokio::test]
    async fn prune_missing_root_is_a_noop() {
        let temp = tempfile::tempdir().expect("temp dir");
        let missing = temp.path().join("missing-cache");
        let report = prune_receive_cache(&missing).await.expect("empty prune");
        assert_eq!(report, Default::default());
        assert!(!missing.exists());
    }

    #[test]
    fn future_manifest_schema_is_rejected() {
        let root = tempfile::tempdir().expect("cache root");
        let options = ReceiveCacheOptions::new(root.path());
        let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .expect("cache lease");
        let entry = lease.entry_dir().to_path_buf();
        lease.preserve().expect("release cache entry");

        let manifest_path = entry.join(CACHE_MANIFEST_FILE);
        let mut manifest: ReceiveCacheManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
                .expect("parse manifest");
        manifest.schema_version = CACHE_SCHEMA_VERSION + 1;
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
        )
        .expect("write future manifest");

        let error = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
            .err()
            .expect("future schema must fail");
        assert!(error.to_string().contains("unsupported"));
    }
}
