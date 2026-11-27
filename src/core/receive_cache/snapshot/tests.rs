use super::*;
use iroh_blobs::api::proto::Bitfield;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::protocol::{ChunkRanges, ChunkRangesSeq};
use iroh_blobs::{Hash, HashAndFormat};

fn sample_record(key: &str) -> RecoveryRecord {
    let mut snapshot = RecoverySnapshot::new(Role::Receiver, TransferSessionId::new());
    snapshot.record_verified_range(0..8);
    snapshot.record_verified_range(16..24);
    snapshot.record_progress(24);
    RecoveryRecord {
        cache_key: key.to_owned(),
        updated_at_unix_seconds: 100,
        transfer_total: 32,
        snapshot,
    }
}

#[test]
fn snapshot_checks_role_key_ttl_terminal_and_ranges() {
    let key = "cache-key";
    let mut record = sample_record(key);
    assert!(record.resumable_at(key, 109, 10));
    assert!(!record.resumable_at(key, 110, 10));
    assert!(!record.resumable_at(key, 99, 10));
    assert!(!record.resumable_at("other-key", 109, 10));
    record.snapshot.role = Role::Sender;
    assert!(!record.resumable_at(key, 109, 10));
    record.snapshot.role = Role::Receiver;
    for terminal in [
        RecoveryTerminalState::Failed,
        RecoveryTerminalState::Cancelled,
        RecoveryTerminalState::Expired,
        RecoveryTerminalState::Completed,
    ] {
        let expected = matches!(
            terminal,
            RecoveryTerminalState::Failed | RecoveryTerminalState::Cancelled
        );
        assert_eq!(record.snapshot.record_terminal_state(terminal), expected);
        assert_eq!(record.resumable_at(key, 109, 10), expected);
    }
    record.snapshot.last_terminal_state = None;
    for invalid in [
        vec![8..8, 16..24],
        vec![8..12, 0..4],
        vec![0..8, 4..16],
        vec![0..8, 8..16],
        vec![0..8, 31..33],
    ] {
        record.snapshot.verified_ranges = invalid;
        assert!(!record.valid_at(key, 109, 10));
    }
    record.snapshot.verified_ranges = vec![0..8, 16..24];
    record.snapshot.processed_bytes = 15;
    assert!(!record.valid_at(key, 109, 10));
    record.snapshot.processed_bytes = 33;
    assert!(!record.valid_at(key, 109, 10));
}

#[test]
fn flatten_missing_preserves_holes_tail_and_logical_offsets() {
    // Root present, first child missing its second 1-KiB chunk, second child
    // entirely missing. A short final chunk is clipped at its verified size.
    let missing_chunks: Bitfield = serde_json::from_str("[4096,1,2]").unwrap();
    let request = GetRequest::new(
        Hash::new(b"root"),
        ChunkRangesSeq::from_ranges([
            ChunkRanges::empty(),
            missing_chunks.ranges,
            ChunkRanges::all(),
        ]),
    );
    let (total, ranges) = ranges_from_missing(&request, &[2500, 30]).unwrap();
    assert_eq!(total, 2594);
    assert_eq!(ranges, vec![0..1088, 2112..2564]);
    assert!(ranges_cover(&ranges, &[64..1024, 2200..2300]));
    assert!(!ranges_cover(&ranges, std::slice::from_ref(&(64..2200))));
    let (total, all_missing) =
        ranges_from_missing(&GetRequest::all(request.hash), &[2500, 30]).unwrap();
    assert_eq!(total, 2594);
    assert!(all_missing.is_empty());
    assert!(ranges_from_missing(&request, &[u64::MAX, 1]).is_err());
}

#[test]
fn atomic_write_replaces_record_and_recovers_abandoned_temp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(SNAPSHOT_FILE);
    let temp = dir.path().join(SNAPSHOT_TEMP);
    let mut record = sample_record("key");
    atomic_write(dir.path(), &record).unwrap();
    std::fs::write(&temp, b"abandoned partial JSON").unwrap();
    assert_eq!(
        read_record(&path).unwrap().unwrap().snapshot,
        record.snapshot
    );
    record
        .snapshot
        .record_terminal_state(RecoveryTerminalState::Cancelled);
    atomic_write(dir.path(), &record).unwrap();
    assert_eq!(
        read_record(&path).unwrap().unwrap().snapshot,
        record.snapshot
    );
    assert!(!temp.exists());
}

#[test]
fn unknown_schema_and_corrupt_snapshot_fall_back_without_removing_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(SNAPSHOT_FILE);
    let blob = dir.path().join("blobs.db");
    std::fs::write(&blob, b"existing verified data").unwrap();
    let key = dir.path().file_name().unwrap().to_str().unwrap();
    let mut record = sample_record(key);
    record.updated_at_unix_seconds = unix_seconds().unwrap();
    for version in [0, RECOVERY_SNAPSHOT_SCHEMA_VERSION + 1] {
        record.snapshot.schema_version = version;
        atomic_write(dir.path(), &record).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read(&blob).unwrap(), b"existing verified data");
    }
    std::fs::write(&path, b"{broken").unwrap();
    let mut journal =
        RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new()).unwrap();
    assert!(journal.previous.is_none());
    journal.finish(RecoveryTerminalState::Failed).unwrap();
    assert!(read_record(&path).unwrap().is_some());
    assert_eq!(std::fs::read(&blob).unwrap(), b"existing verified data");
}

#[test]
fn expired_completed_and_wrong_key_snapshots_are_not_reused() {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().file_name().unwrap().to_str().unwrap();
    let mut record = sample_record(key);
    record.updated_at_unix_seconds = 0;
    atomic_write(dir.path(), &record).unwrap();
    assert!(
        RecoveryJournal::open_enabled(dir.path(), 1, TransferSessionId::new())
            .unwrap()
            .previous
            .is_none()
    );
    record.updated_at_unix_seconds = unix_seconds().unwrap();
    record
        .snapshot
        .record_terminal_state(RecoveryTerminalState::Completed);
    atomic_write(dir.path(), &record).unwrap();
    assert!(
        RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new())
            .unwrap()
            .previous
            .is_none()
    );
    record.snapshot.last_terminal_state = None;
    record.cache_key = "another-key".into();
    atomic_write(dir.path(), &record).unwrap();
    assert!(
        RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new())
            .unwrap()
            .previous
            .is_none()
    );
}

#[tokio::test]
async fn snapshot_reopens_against_real_store_without_importing_old_session() {
    let dir = tempfile::tempdir().unwrap();
    let store = iroh_blobs::store::fs::FsStore::load(dir.path())
        .await
        .unwrap();
    let child = store
        .add_bytes(b"payload".as_slice())
        .temp_tag()
        .await
        .unwrap();
    let mut collection = Collection::default();
    collection.push("payload.txt".into(), child.hash());
    let root = collection.store(&store).await.unwrap();
    let content = HashAndFormat::hash_seq(root.hash());
    let local = store.remote().local(content).await.unwrap();
    assert!(local.is_complete());
    let id = TransferSessionId::new();
    let mut first = RecoveryJournal::open_enabled(dir.path(), 60, id.clone()).unwrap();
    assert!(first.checkpoint(&local, None).unwrap());
    first.finish(RecoveryTerminalState::Failed).unwrap();
    let path = dir.path().join(SNAPSHOT_FILE);
    let first_record = read_record(&path).unwrap().unwrap();
    assert_eq!(first_record.snapshot.session_id, id);
    assert_eq!(first_record.snapshot.verified_bytes(), local.local_bytes());
    assert!(first_record.snapshot.can_resume());

    let next_id = TransferSessionId::new();
    let mut next = RecoveryJournal::open_enabled(dir.path(), 60, next_id.clone()).unwrap();
    assert!(next.previous.is_some());
    assert!(next.checkpoint(&local, None).unwrap());
    assert!(next.previous.is_none());
    let next_record = read_record(&path).unwrap().unwrap();
    assert_eq!(next_record.snapshot.session_id, next_id);
    assert_eq!(next_record.snapshot.last_terminal_state, None);
    assert_eq!(
        next_record.snapshot.verified_ranges,
        first_record.snapshot.verified_ranges
    );
    next.finish(RecoveryTerminalState::Completed).unwrap();
    assert!(!read_record(&path).unwrap().unwrap().snapshot.can_resume());
    store.shutdown().await.unwrap();
}

#[cfg(unix)]
#[test]
fn snapshot_symlinks_do_not_modify_external_files() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(outside.path(), b"untouched").unwrap();
    symlink(outside.path(), dir.path().join(SNAPSHOT_FILE)).unwrap();
    assert!(RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new()).is_err());
    assert!(atomic_write(dir.path(), &sample_record("key")).is_err());
    assert_eq!(std::fs::read(outside.path()).unwrap(), b"untouched");
}

#[tokio::test]
async fn forged_complete_snapshot_is_rebuilt_from_an_incomplete_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = iroh_blobs::store::fs::FsStore::load(dir.path())
        .await
        .unwrap();
    let absent_data = b"not yet present in the receiving store";
    let absent_hash = Hash::new(absent_data);
    let mut collection = Collection::default();
    collection.push("missing.txt".into(), absent_hash);
    let root = collection.store(&store).await.unwrap();
    let content = HashAndFormat::hash_seq(root.hash());
    let local = store.remote().local(content).await.unwrap();
    assert!(!local.is_complete());
    let root_bytes = store.get_bytes(root.hash()).await.unwrap();
    let links = iroh_blobs::hashseq::HashSeq::new(root_bytes).unwrap();
    let metadata = store.get_bytes(links.get(0).unwrap()).await.unwrap();
    let sizes = [metadata.len() as u64, absent_data.len() as u64];
    let total = root_bytes_len(&sizes);
    let key = dir.path().file_name().unwrap().to_str().unwrap();
    let mut forged = sample_record(key);
    forged.updated_at_unix_seconds = unix_seconds().unwrap();
    forged.transfer_total = total;
    forged.snapshot.verified_ranges = std::iter::once(0..total).collect();
    forged.snapshot.processed_bytes = total;
    atomic_write(dir.path(), &forged).unwrap();

    let mut journal =
        RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new()).unwrap();
    assert!(
        journal.previous.is_some(),
        "forged record is structurally valid"
    );
    journal.set_layout(sizes.into());
    assert!(journal.checkpoint(&local, None).unwrap());
    let rebuilt = read_record(&dir.path().join(SNAPSHOT_FILE))
        .unwrap()
        .unwrap();
    assert_eq!(rebuilt.snapshot.verified_bytes(), local.local_bytes());
    assert!(rebuilt.snapshot.verified_bytes() < total);
    assert_eq!(
        rebuilt.snapshot.verified_ranges,
        std::iter::once(0..local.local_bytes()).collect::<Vec<_>>()
    );
    assert_eq!(
        local.missing(),
        store.remote().local(content).await.unwrap().missing()
    );
    store.shutdown().await.unwrap();
}

fn root_bytes_len(sizes: &[u64]) -> u64 {
    sizes.len() as u64 * 32 + sizes.iter().sum::<u64>()
}

#[tokio::test]
async fn only_fresh_successfully_committed_complete_checkpoint_can_be_reused() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let store = iroh_blobs::store::fs::FsStore::load(store_dir.path())
        .await
        .unwrap();
    let child = store
        .add_bytes(b"payload".as_slice())
        .temp_tag()
        .await
        .unwrap();
    let mut collection = Collection::default();
    collection.push("payload.txt".into(), child.hash());
    let root = collection.store(&store).await.unwrap();
    let local = store
        .remote()
        .local(HashAndFormat::hash_seq(root.hash()))
        .await
        .unwrap();
    assert!(local.is_complete());
    store.sync_db().await.unwrap();
    let mut journal =
        RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new()).unwrap();
    assert!(!journal.has_fresh_complete_checkpoint_at(journal.record.updated_at_unix_seconds));
    assert!(journal.checkpoint(&local, None).unwrap());
    let committed_at = journal.record.updated_at_unix_seconds;
    assert!(journal.has_fresh_complete_checkpoint_at(committed_at));
    assert!(!journal.has_fresh_complete_checkpoint_at(committed_at + 1));
    assert!(!journal.has_fresh_complete_checkpoint_at(committed_at - 1));

    // Reopening a complete record does not make it evidence in the new session.
    let mut reopened =
        RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new()).unwrap();
    assert!(reopened.previous.is_some());
    assert!(!reopened.has_fresh_complete_checkpoint_at(reopened.record.updated_at_unix_seconds));

    // An observation/barrier attempt invalidates the proof even if it later fails.
    journal.invalidate_complete_checkpoint();
    assert!(!journal.has_fresh_complete_checkpoint_at(committed_at));
    assert!(journal.checkpoint(&local, None).unwrap());
    assert!(journal.has_fresh_complete_checkpoint_at(journal.record.updated_at_unix_seconds));

    // A new layout or an incomplete observation must revoke the optimization.
    journal.set_layout(Arc::from([1_u64]));
    assert!(!journal.has_fresh_complete_checkpoint_at(committed_at));
    let absent = store
        .remote()
        .local(HashAndFormat::hash_seq(Hash::new(b"absent")))
        .await
        .unwrap();
    assert!(!absent.is_complete());
    assert!(!reopened.checkpoint(&absent, None).unwrap());
    assert!(!reopened.has_fresh_complete_checkpoint_at(reopened.record.updated_at_unix_seconds));

    assert!(reopened.checkpoint(&local, None).unwrap());
    reopened.finish(RecoveryTerminalState::Failed).unwrap();
    assert!(!reopened.has_fresh_complete_checkpoint_at(reopened.record.updated_at_unix_seconds));
    store.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_checkpoint_write_cannot_mark_complete() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = tempfile::tempdir().unwrap();
    let store = iroh_blobs::store::fs::FsStore::load(store_dir.path())
        .await
        .unwrap();
    let tag = store
        .add_bytes(b"payload".as_slice())
        .temp_tag()
        .await
        .unwrap();
    let local = store
        .remote()
        .local(HashAndFormat::raw(tag.hash()))
        .await
        .unwrap();
    assert!(local.is_complete());
    store.sync_db().await.unwrap();
    let mut journal =
        RecoveryJournal::open_enabled(dir.path(), 60, TransferSessionId::new()).unwrap();
    std::fs::create_dir(dir.path().join(SNAPSHOT_TEMP)).unwrap();
    assert!(journal.checkpoint(&local, None).is_err());
    assert!(!journal.has_fresh_complete_checkpoint_at(journal.record.updated_at_unix_seconds));
    store.shutdown().await.unwrap();
}
