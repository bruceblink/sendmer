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
pub fn prune_cache_root(cache_root: &Path) -> anyhow::Result<ReceiveCachePruneReport> {
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

    let root_lock = lock_root_exclusive(cache_root)?;

    let scan_result = prune_cache_entries(&layout_dir);
    let unlock_result =
        FileExt::unlock(&root_lock).context("unlock receive cache root after prune");
    match (scan_result, unlock_result) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(report), Ok(())) => Ok(report),
    }
}

/// 取得缓存根的排他维护锁，统一串行化 prune、删除和新 receive 的目录变更。
fn lock_root_exclusive(cache_root: &Path) -> anyhow::Result<File> {
    try_lock_root_exclusive(cache_root)?
        .ok_or_else(|| anyhow::anyhow!("receive cache maintenance is already in progress"))
}

/// 取得 root shared 锁，保证容量统计不会和 prune/删除并发修改目录。
pub fn lock_root_shared(cache_root: &Path) -> anyhow::Result<File> {
    let root_lock = open_lock_file(&cache_root.join(CACHE_ROOT_LOCK_FILE))?;
    match FileExt::try_lock_shared(&root_lock) {
        Ok(()) => Ok(root_lock),
        Err(TryLockError::WouldBlock) => {
            anyhow::bail!("receive cache maintenance is already in progress")
        }
        Err(TryLockError::Error(error)) => Err(error).context("lock receive cache root"),
    }
}

pub fn try_lock_root_exclusive(cache_root: &Path) -> anyhow::Result<Option<File>> {
    let root_lock = open_lock_file(&cache_root.join(CACHE_ROOT_LOCK_FILE))?;
    match FileExt::try_lock(&root_lock) {
        Ok(()) => Ok(Some(root_lock)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error).context("lock receive cache root"),
    }
}

/// 检查现有缓存占用和文件系统余量；调用方应先持有 root shared/exclusive 锁。
pub fn ensure_cache_capacity(cache_root: &Path) -> anyhow::Result<()> {
    let free_bytes = available_disk_space(cache_root)?;
    ensure_cache_capacity_with_free_space(cache_root, free_bytes)
}

/// 可注入可用空间的容量检查，便于在没有真实低磁盘环境时测试拒绝逻辑。
pub fn ensure_cache_capacity_with_free_space(
    cache_root: &Path,
    free_bytes: u64,
) -> anyhow::Result<()> {
    let used_bytes = cache_directory_size(cache_root, CACHE_MAX_BYTES)?;
    anyhow::ensure!(
        used_bytes <= CACHE_MAX_BYTES,
        "receive cache exceeds the {}-byte hard limit",
        CACHE_MAX_BYTES
    );
    anyhow::ensure!(
        free_bytes >= CACHE_MIN_FREE_BYTES,
        "receive cache requires at least {} bytes of free disk space",
        CACHE_MIN_FREE_BYTES
    );
    Ok(())
}

/// 只统计普通文件，并在达到上限时立即失败；不跟随缓存中的符号链接。
pub fn cache_directory_size(root: &Path, limit: u64) -> anyhow::Result<u64> {
    let mut directories = vec![root.to_path_buf()];
    let mut total = 0_u64;
    while let Some(directory) = directories.pop() {
        for item in std::fs::read_dir(&directory).context("scan receive cache capacity")? {
            let item = item.context("read receive cache capacity entry")?;
            let metadata = std::fs::symlink_metadata(item.path())
                .context("inspect receive cache capacity entry")?;
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                directories.push(item.path());
                continue;
            }
            if file_type.is_file() {
                total = total
                    .checked_add(metadata.len())
                    .ok_or_else(|| anyhow::anyhow!("receive cache size overflow"))?;
                anyhow::ensure!(
                    total <= limit,
                    "receive cache exceeds the configured capacity limit"
                );
            }
        }
    }
    Ok(total)
}

#[cfg(unix)]
fn available_disk_space(path: &Path) -> anyhow::Result<u64> {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .context("encode receive cache path for disk-space check")?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `stats` points to writable storage and the path is NUL-terminated.
    let result = unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) };
    anyhow::ensure!(
        result == 0,
        "inspect free disk space for receive cache: {}",
        std::io::Error::last_os_error()
    );
    // SAFETY: statvfs initialized `stats` when it returned success.
    let stats = unsafe { stats.assume_init() };
    let bytes = u128::from(stats.f_bavail).saturating_mul(u128::from(stats.f_frsize));
    Ok(bytes.min(u128::from(u64::MAX)) as u64)
}

#[cfg(windows)]
fn available_disk_space(path: &Path) -> anyhow::Result<u64> {
    use std::iter::once;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let path: Vec<u16> = path.as_os_str().encode_wide().chain(once(0)).collect();
    let mut free_bytes = 0_u64;
    // SAFETY: all output pointers refer to writable local values and `path` is
    // NUL-terminated for the Windows API.
    let result = unsafe {
        GetDiskFreeSpaceExW(
            path.as_ptr(),
            &mut free_bytes,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    anyhow::ensure!(
        result != 0,
        "inspect free disk space for receive cache: {}",
        std::io::Error::last_os_error()
    );
    Ok(free_bytes)
}

#[cfg(not(any(unix, windows)))]
fn available_disk_space(_path: &Path) -> anyhow::Result<u64> {
    anyhow::bail!("receive cache disk-space checks are unsupported on this platform")
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
    let manifest_path = entry_dir.join(CACHE_MANIFEST_FILE);
    let metadata = match std::fs::symlink_metadata(&manifest_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect receive cache manifest during prune"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    let manifest: ReceiveCacheManifest = match std::fs::read(&manifest_path) {
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

pub fn open_existing_lock_file(path: &Path) -> anyhow::Result<Option<File>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Ok(None);
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect receive cache heartbeat"),
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .context("open receive cache heartbeat")?;
    let metadata = std::fs::symlink_metadata(path).context("inspect receive cache heartbeat")?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "receive cache heartbeat must be a plain file"
    );
    Ok(Some(file))
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
pub fn prepare_cache_root(root: &Path) -> anyhow::Result<PathBuf> {
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
pub fn prepare_plain_directory(path: &Path) -> anyhow::Result<(PathBuf, bool)> {
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

pub fn open_lock_file(path: &Path) -> anyhow::Result<File> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => anyhow::ensure!(
            !metadata.file_type().is_symlink(),
            "receive cache lock must not be a symbolic link"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect receive cache lock"),
    }

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .context("open receive cache lock")?;
    let metadata = std::fs::symlink_metadata(path).context("inspect receive cache lock")?;
    anyhow::ensure!(
        !metadata.file_type().is_symlink() && metadata.is_file(),
        "receive cache lock must be a plain file"
    );
    Ok(file)
}

/// Write a heartbeat timestamp without replacing the inode used for locking.
pub fn touch_lock_file(mut file: &File) -> anyhow::Result<()> {
    let now = unix_seconds()?;
    file.set_len(0).context("truncate receive cache lock")?;
    file.seek(SeekFrom::Start(0))
        .context("seek receive cache lock")?;
    writeln!(file, "{now}").context("refresh receive cache lock")?;
    file.sync_data().context("sync receive cache lock")
}

pub fn validate_or_create_manifest(
    entry_dir: &Path,
    expected_key: &str,
    ttl_seconds: u64,
    entry_created: bool,
) -> anyhow::Result<u64> {
    let manifest_path = entry_dir.join(CACHE_MANIFEST_FILE);
    match std::fs::symlink_metadata(&manifest_path) {
        Ok(metadata) => {
            anyhow::ensure!(
                !metadata.file_type().is_symlink(),
                "receive cache manifest must not be a symbolic link"
            );
            anyhow::ensure!(
                metadata.is_file(),
                "receive cache manifest must be a plain file"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect receive cache manifest"),
    }
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
            Ok(manifest.ttl_seconds)
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
            write_new_manifest(&manifest_path, &manifest)?;
            Ok(ttl_seconds)
        }
        Err(error) => Err(error).context("read receive cache manifest"),
    }
}

fn cache_entry_has_only_control_files(entry_dir: &Path) -> anyhow::Result<bool> {
    for item in std::fs::read_dir(entry_dir).context("inspect receive cache entry")? {
        let item = item.context("read receive cache entry")?;
        let name = item.file_name();
        let name = name.to_string_lossy();
        let file_type = item
            .file_type()
            .context("inspect receive cache control file")?;
        if name == CACHE_LOCK_FILE {
            if file_type.is_symlink() || !file_type.is_file() {
                return Ok(false);
            }
            continue;
        }
        if name.starts_with(MANIFEST_TEMP_PREFIX) {
            anyhow::ensure!(
                !file_type.is_symlink() && file_type.is_file(),
                "receive cache manifest temp must be a plain file"
            );
            continue;
        }
        if name != CACHE_LOCK_FILE {
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
            let file_type = item
                .file_type()
                .context("inspect receive cache manifest temp")?;
            anyhow::ensure!(
                !file_type.is_symlink() && file_type.is_file(),
                "receive cache manifest temp must be a plain file"
            );
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

pub fn cache_key(hash_and_format: HashAndFormat) -> String {
    format!(
        "{}-{}",
        u64::from(hash_and_format.format),
        hash_and_format.hash.to_hex()
    )
}

pub fn unix_seconds() -> anyhow::Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")
        .map(|duration| duration.as_secs())
}

use super::model::*;
use anyhow::Context;
use fs4::{FileExt, TryLockError};
use iroh_blobs::HashAndFormat;
use rand::Rng;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
