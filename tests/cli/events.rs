use super::{RunningSend, assert_ordered_single_session, parse_json_events, sendmer_bin};
use iroh::EndpointAddr;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, Hash};
use sendmer::{TransferErrorCode, TransferEventData};
use std::process::{Command, Stdio};
#[test]
fn json_events_keep_piped_stdout_machine_readable_on_receive_failure() {
    let output_dir = tempfile::tempdir().unwrap();
    let secret = sendmer::endpoint_secret().unwrap();
    let ticket = BlobTicket::new(
        EndpointAddr::new(secret.public()),
        Hash::new(b"sendmer-json-failure-test"),
        BlobFormat::HashSeq,
    );
    let output = Command::new(sendmer_bin())
        .args([
            "receive",
            "--json-events",
            "--relay",
            "disabled",
            "--retry-limit",
            "1",
            "--retry-backoff-ms",
            "0",
            "--output-dir",
        ])
        .arg(output_dir.path())
        .arg(ticket.to_string())
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run receive failure command");

    assert!(!output.status.success());
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    assert!(matches!(
        &events.last().expect("failed event").event,
        TransferEventData::Failed { error }
            if error.code == TransferErrorCode::ConnectionFailed && error.retryable
    ));
    assert!(!output.stderr.is_empty(), "diagnostics should use stderr");
}

#[test]
fn json_events_keep_piped_stdout_machine_readable_on_send_failure() {
    let source_root = tempfile::tempdir().unwrap();
    let empty_dir = source_root.path().join("empty");
    std::fs::create_dir(&empty_dir).unwrap();
    let output = Command::new(sendmer_bin())
        .args(["send", "--json-events", "--relay", "disabled"])
        .arg(&empty_dir)
        .current_dir(source_root.path())
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run send failure command");

    assert!(!output.status.success());
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    assert!(matches!(
        &events.last().expect("failed event").event,
        TransferEventData::Failed { error }
            if error.code == TransferErrorCode::InvalidInput && !error.retryable
    ));
    assert!(!output.stderr.is_empty(), "diagnostics should use stderr");
}

#[test]
fn json_events_report_file_limit_before_sender_startup() {
    let source_root = tempfile::tempdir().unwrap();
    let share = source_root.path().join("share");
    std::fs::create_dir(&share).unwrap();
    std::fs::write(share.join("one.txt"), b"one").unwrap();
    std::fs::write(share.join("two.txt"), b"two").unwrap();
    let output = Command::new(sendmer_bin())
        .args([
            "send",
            "--json-events",
            "--relay",
            "disabled",
            "--max-files",
            "1",
        ])
        .arg(&share)
        .current_dir(source_root.path())
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run file limit command");

    assert!(!output.status.success());
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    assert!(matches!(
        &events.last().expect("failed event").event,
        TransferEventData::Failed { error }
            if error.code == TransferErrorCode::InvalidInput
                && error.phase == sendmer::TransferPhase::Preparing
                && !error.retryable
    ));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("more than 1 files"),
        "diagnostics should explain the file limit"
    );
}

#[test]
fn json_events_report_total_size_limit_before_sender_startup() {
    let source_root = tempfile::tempdir().unwrap();
    let share = source_root.path().join("share");
    std::fs::create_dir(&share).unwrap();
    std::fs::write(share.join("payload.bin"), b"payload").unwrap();
    let output = Command::new(sendmer_bin())
        .args([
            "send",
            "--json-events",
            "--relay",
            "disabled",
            "--max-total-size",
            "6",
        ])
        .arg(&share)
        .current_dir(source_root.path())
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run total size limit command");

    assert!(!output.status.success());
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    assert!(matches!(
        &events.last().expect("failed event").event,
        TransferEventData::Failed { error }
            if error.code == TransferErrorCode::InvalidInput
                && error.phase == sendmer::TransferPhase::Preparing
                && !error.retryable
    ));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("more than 6 bytes"),
        "diagnostics should explain the total size limit"
    );
}

#[test]
fn json_events_report_success_without_human_text_on_stdout() {
    let name = "json-success.bin";
    let data = vec![5u8; 32 * 1024];
    let src_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();
    let mut send = RunningSend::spawn(&src_file, src_dir.path()).unwrap();
    let ticket = send.read_ticket();

    let output = Command::new(sendmer_bin())
        .args([
            "receive",
            "--json-events",
            "--relay",
            "disabled",
            "--output-dir",
        ])
        .arg(target_dir.path())
        .arg(ticket.to_string())
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run successful JSON receive");
    send.cleanup();

    assert!(
        output.status.success(),
        "receive failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Completed)
    ));
    assert_eq!(std::fs::read(target_dir.path().join(name)).unwrap(), data);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Downloaded"),
        "human result should use stderr in JSON mode"
    );
}
