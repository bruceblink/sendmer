use super::{RunningSend, sendmer_bin};
use iroh_blobs::ticket::BlobTicket;
use sendmer::{RecoverySnapshot, RecoveryTerminalState, Role};
use std::path::Path;
use std::process::{Child, Command, Output};
use std::time::{Duration, Instant};

pub(super) fn read_snapshot(entry: &Path) -> RecoverySnapshot {
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(entry.join("recovery.json")).expect("read committed recovery record"),
    )
    .expect("parse recovery record");
    assert_eq!(
        value["cache_key"],
        entry.file_name().unwrap().to_str().unwrap()
    );
    let total = value["transfer_total"].as_u64().unwrap();
    let snapshot: RecoverySnapshot = serde_json::from_value(value["snapshot"].clone()).unwrap();
    assert_eq!(snapshot.role, Role::Receiver);
    assert!(snapshot.processed_bytes <= total);
    assert!(snapshot.verified_bytes() <= snapshot.processed_bytes);
    assert!(
        snapshot
            .verified_ranges
            .iter()
            .all(|r| !r.is_empty() && r.end <= total)
    );
    snapshot
}

/// Wait for committed recovery metadata, not a fixed delay after telemetry.
pub(super) fn wait_for_live_checkpoint(entry: &Path, child: &mut Child, minimum: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            child.try_wait().expect("inspect receiver").is_none(),
            "receiver exited before writing an active checkpoint"
        );
        if let Ok(bytes) = std::fs::read(entry.join("recovery.json"))
            && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Ok(snapshot) =
                serde_json::from_value::<RecoverySnapshot>(value["snapshot"].clone())
            && snapshot.last_terminal_state.is_none()
            && snapshot.can_resume()
            && snapshot.verified_bytes() >= minimum
        {
            return;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("receiver did not persist the required verified checkpoint before deadline");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn receive(ticket: &BlobTicket, cache: &Path, output: &Path, disable: bool) -> Output {
    let mut cmd = Command::new(sendmer_bin());
    cmd.args([
        "receive",
        "--relay",
        "disabled",
        "--magic-ipv4-addr",
        "127.0.0.1:0",
        "--no-progress",
        "--cache-dir",
        cache.to_str().unwrap(),
        "--output-dir",
        output.to_str().unwrap(),
        &ticket.to_string(),
    ])
    .env_remove("RUST_LOG");
    if disable {
        cmd.env("SENDMER_DISABLE_RECOVERY_SNAPSHOT", "1");
    } else {
        cmd.env_remove("SENDMER_DISABLE_RECOVERY_SNAPSHOT");
    }
    cmd.output().unwrap()
}

#[test]
fn unsupported_corrupt_and_disabled_snapshots_preserve_real_blob_recovery() {
    let source = tempfile::tempdir().unwrap();
    let collision = tempfile::tempdir().unwrap();
    let output = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let name = "snapshot-fallback.bin";
    let data = vec![42u8; 128 * 1024];
    let path = source.path().join(name);
    std::fs::write(&path, &data).unwrap();
    std::fs::write(collision.path().join(name), b"existing").unwrap();
    let mut sender = RunningSend::spawn(&path, source.path()).unwrap();
    let ticket = sender.read_ticket();
    let entry = cache
        .path()
        .join("v1")
        .join(format!("1-{}", ticket.hash().to_hex()));
    let record = entry.join("recovery.json");
    let first = receive(&ticket, cache.path(), collision.path(), false);
    assert!(!first.status.success());
    let first_snapshot = read_snapshot(&entry);
    assert!(first_snapshot.can_resume());
    assert_eq!(
        first_snapshot.last_terminal_state,
        Some(RecoveryTerminalState::Failed)
    );
    // All payload is already verified. Further receives must not depend on the sender
    // or on potentially damaged recovery metadata.
    sender.cleanup();

    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    value["snapshot"]["schema_version"] = serde_json::json!(0);
    let old = serde_json::to_vec(&value).unwrap();
    std::fs::write(&record, &old).unwrap();
    assert!(
        !receive(&ticket, cache.path(), collision.path(), false)
            .status
            .success()
    );
    assert_eq!(std::fs::read(&record).unwrap(), old);
    assert!(entry.join("blobs.db").is_file());

    std::fs::write(&record, b"{invalid").unwrap();
    assert!(
        !receive(&ticket, cache.path(), collision.path(), false)
            .status
            .success()
    );
    let rebuilt = read_snapshot(&entry);
    assert_eq!(
        rebuilt.last_terminal_state,
        Some(RecoveryTerminalState::Failed)
    );
    assert_ne!(rebuilt.session_id, first_snapshot.session_id);
    assert_eq!(rebuilt.verified_ranges, first_snapshot.verified_ranges);
    assert_eq!(
        std::fs::read(collision.path().join(name)).unwrap(),
        b"existing"
    );

    // The rollback switch suppresses both read and write, retaining existing data.
    let previous = std::fs::read(&record).unwrap();
    assert!(
        !receive(&ticket, cache.path(), collision.path(), true)
            .status
            .success()
    );
    assert_eq!(std::fs::read(&record).unwrap(), previous);
    let resumed = receive(&ticket, cache.path(), output.path(), true);
    assert!(
        resumed.status.success(),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(std::fs::read(output.path().join(name)).unwrap(), data);
    assert!(!entry.exists());
}
