use super::{
    CACHE_MANIFEST_FILE, CACHE_MIN_FREE_BYTES, CACHE_ROOT_LOCK_FILE, CACHE_SCHEMA_VERSION,
    ReceiveCacheLease, ReceiveCacheManifest, cache_directory_size, cache_key,
    ensure_cache_capacity_with_free_space, open_lock_file, prune_receive_cache,
};
use crate::core::options::ReceiveCacheOptions;
use fs4::FileExt;
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

#[test]
fn preserve_does_not_wait_with_entry_before_busy_root_lock() {
    let root = tempfile::tempdir().expect("cache root");
    let options = ReceiveCacheOptions::new(root.path());
    let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
        .expect("cache lease");
    let root_lock = open_lock_file(&root.path().join(CACHE_ROOT_LOCK_FILE)).expect("root lock");
    FileExt::try_lock(&root_lock).expect("hold root maintenance lock");

    let result = lease.preserve();

    assert!(
        result.is_err(),
        "busy root lock should fail without waiting"
    );
    FileExt::unlock(&root_lock).expect("release root maintenance lock");
    ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
        .expect("entry lock must be released after preserve failure")
        .preserve()
        .expect("release reopened cache entry");
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
async fn remove_keeps_entry_when_root_lock_is_busy() {
    let root = tempfile::tempdir().expect("cache root");
    let options = ReceiveCacheOptions::new(root.path());
    let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
        .expect("cache lease");
    let entry = lease.entry_dir().to_path_buf();
    let root_lock_path = root.path().join(CACHE_ROOT_LOCK_FILE);
    let root_lock = open_lock_file(&root_lock_path).expect("root lock");
    FileExt::try_lock(&root_lock).expect("hold root maintenance lock");

    let result = lease.remove().await;

    assert!(
        result.is_ok(),
        "remove must preserve the entry during maintenance"
    );
    assert!(entry.exists(), "busy root lock must preserve the entry");
    FileExt::unlock(&root_lock).expect("release root maintenance lock");
}

#[test]
fn cache_capacity_rejects_low_space_and_bounded_size() {
    let root = tempfile::tempdir().expect("cache root");
    std::fs::write(root.path().join("data"), b"1234").expect("cache data");

    let size_error = cache_directory_size(root.path(), 3).expect_err("size limit");
    assert!(size_error.to_string().contains("capacity limit"));

    let low_space = ensure_cache_capacity_with_free_space(root.path(), CACHE_MIN_FREE_BYTES - 1)
        .expect_err("low free space");
    assert!(low_space.to_string().contains("free disk space"));

    ensure_cache_capacity_with_free_space(root.path(), CACHE_MIN_FREE_BYTES)
        .expect("capacity within hard limit");
}

#[cfg(unix)]
#[test]
fn manifest_symlink_is_rejected_when_opening_entry() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("cache root");
    let options = ReceiveCacheOptions::new(root.path());
    let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
        .expect("cache lease");
    let entry = lease.entry_dir().to_path_buf();
    lease.preserve().expect("release cache entry");

    let manifest = entry.join(CACHE_MANIFEST_FILE);
    let target = root.path().join("manifest-target");
    std::fs::copy(&manifest, &target).expect("copy manifest target");
    std::fs::remove_file(&manifest).expect("remove manifest");
    symlink(&target, &manifest).expect("manifest symlink");

    let error = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
        .err()
        .expect("manifest symlink must fail closed");
    assert!(
        error
            .to_string()
            .contains("manifest must not be a symbolic link")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn prune_preserves_manifest_symlink_entry() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("cache root");
    let options = ReceiveCacheOptions::new(root.path());
    let lease = ReceiveCacheLease::open(&options, hash_and_format(BlobFormat::HashSeq))
        .expect("cache lease");
    let entry = lease.entry_dir().to_path_buf();
    lease.preserve().expect("release cache entry");

    let manifest = entry.join(CACHE_MANIFEST_FILE);
    let target = root.path().join("manifest-target");
    std::fs::copy(&manifest, &target).expect("copy manifest target");
    std::fs::remove_file(&manifest).expect("remove manifest");
    symlink(&target, &manifest).expect("manifest symlink");
    std::fs::write(entry.join(".lock"), b"0\n").expect("expire cache heartbeat");

    let report = prune_receive_cache(root.path()).await.expect("prune cache");

    assert_eq!(report.removed_entries, 0);
    assert_eq!(report.unknown_entries, 1);
    assert!(entry.exists());
}

#[tokio::test]
async fn prune_removes_expired_unlocked_entry() {
    let root = tempfile::tempdir().expect("cache root");
    let options = ReceiveCacheOptions::new(root.path()).with_ttl(std::time::Duration::from_secs(1));
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
    std::fs::create_dir_all(root.path().join("v1").join("not-an-entry")).expect("unknown entry");

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
