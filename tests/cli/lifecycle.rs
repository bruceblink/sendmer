#[cfg(feature = "persistent-recovery")]
use super::{RecordingEmitter, recovery::read_snapshot};
use super::{
    RunningSend, assert_ordered_single_session, ephemeral_loopback_addr, parse_json_events,
    sendmer_bin,
};
use iroh_blobs::ticket::BlobTicket;
use sendmer::{TransferErrorCode, TransferEvent, TransferEventData};
use std::io::{BufRead, BufReader, Read};
#[cfg(feature = "persistent-recovery")]
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::Path;
use std::process::{Child, Command, Stdio};
#[cfg(feature = "persistent-recovery")]
use std::sync::Arc;
use std::time::{Duration, Instant};
#[cfg(feature = "persistent-recovery")]
#[test]
fn cancellable_receive_preserves_cache_and_emits_one_cancelled_terminal() {
    let name = "cancelled-receive.bin";
    let data = vec![13u8; 2 * 1024 * 1024];
    let source_dir = tempfile::tempdir().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let cache_root = tempfile::tempdir().unwrap();
    let source_file = source_dir.path().join(name);
    std::fs::write(&source_file, &data).unwrap();

    let mut send =
        RunningSend::spawn_with_upload_rate(&source_file, source_dir.path(), Some(128 * 1024))
            .unwrap();
    let ticket = send.read_ticket();
    let cache_entry = cache_root
        .path()
        .join("v1")
        .join(format!("1-{}", ticket.hash().to_hex()));
    let emitter = Arc::new(RecordingEmitter::default());
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let options = sendmer::ReceiveOptions {
        output_dir: Some(output_dir.path().to_path_buf()),
        relay_mode: sendmer::RelayModeOption::Disabled,
        magic_ipv4_addr: Some(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)),
        retry_policy: Default::default(),
        receive_cache: Some(sendmer::ReceiveCacheOptions::new(cache_root.path())),
        ..Default::default()
    };
    let mut receiver = runtime.spawn(sendmer::receive_with_events_and_cancellation(
        ticket.to_string(),
        options,
        Some(emitter.clone()),
        Some(cancel_rx),
    ));

    // Wait for actual payload progress before cancelling, so this covers the receive cleanup
    // path after a cache entry has been opened and verified data has reached disk.
    let progress_wait = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if emitter.snapshot().iter().any(|event| {
                    matches!(
                        &event.event,
                        TransferEventData::Progress { processed, .. } if *processed > 0
                    )
                }) {
                    return Ok(());
                }
                tokio::select! {
                    result = &mut receiver => {
                        return Err((result, emitter.snapshot()));
                    }
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                }
            }
        })
        .await
    });
    match progress_wait {
        Ok(Ok(())) => {}
        Ok(Err((result, events))) => match result {
            Ok(Ok(_)) => {
                panic!("receive completed before payload progress: structured events={events:?}")
            }
            Ok(Err(_)) => panic!(
                "receive returned an error before payload progress: structured events={events:?}"
            ),
            Err(join_error) => panic!(
                "receive task join failed before payload progress: {join_error}; structured events={events:?}"
            ),
        },
        Err(error) => {
            let events = emitter.snapshot();
            panic!(
                "receive made no payload progress within 30 seconds: {error}; events={events:?}"
            );
        }
    }

    runtime.block_on(async {
        cancel_tx.send(true).expect("send receive cancellation");
        let result = receiver.await.expect("receive task should join");
        assert!(result.is_err(), "cancelled receive must return an error");
    });
    send.cleanup();

    assert!(cache_entry.join("manifest.json").is_file());
    assert!(cache_entry.join("blobs.db").is_file());
    assert!(!output_dir.path().join(name).exists());
    let events = emitter.snapshot();
    let snapshot = read_snapshot(&cache_entry);
    assert_eq!(snapshot.session_id, events[0].session_id);
    assert_eq!(
        snapshot.last_terminal_state,
        Some(sendmer::RecoveryTerminalState::Cancelled)
    );
    assert_ordered_single_session(&events);
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Cancelled)
    ));

    let report = runtime
        .block_on(sendmer::prune_receive_cache(cache_root.path()))
        .expect("cancelled receive must release the cache lock");
    assert_eq!(report.retained_entries, 1);
}

#[test]
fn cli_session_lifetime_expires_and_cleans_sender_storage() {
    let source_dir = tempfile::tempdir().unwrap();
    let isolated_temp = tempfile::tempdir().unwrap();
    let source_file = source_dir.path().join("session-lifetime.bin");
    std::fs::write(&source_file, b"session lifetime cleanup").unwrap();
    let temp_path = isolated_temp.path().to_str().unwrap();

    let output = Command::new(sendmer_bin())
        .args([
            "send",
            "--no-progress",
            "--json-events",
            "--relay",
            "disabled",
            "--session-lifetime-seconds",
            "1",
        ])
        .arg(&source_file)
        .current_dir(source_dir.path())
        .env("TMPDIR", temp_path)
        .env("TEMP", temp_path)
        .env("TMP", temp_path)
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();

    // The CLI should treat an expected lifetime expiry as a clean shutdown while still
    // publishing the structured timeout event that explains why the share ended.
    assert!(
        output.status.success(),
        "expired sender should exit cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Failed { error })
            if error.code == TransferErrorCode::Timeout
                && error.phase == sendmer::TransferPhase::Finalizing
                && !error.retryable
    ));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("sendmer receive"),
        "JSON mode should keep the receive command in stderr"
    );

    let leaked = std::fs::read_dir(isolated_temp.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".sendmer-send-"))
        })
        .collect::<Vec<_>>();
    assert!(
        leaked.is_empty(),
        "sender lifetime shutdown should remove temporary storage: {leaked:?}"
    );
}

/// Exercise the public sender handle while an external CLI receiver is actively downloading.
#[test]
fn send_handle_cancel_stops_active_cli_receive_without_export() {
    let name = "cancelled-sender.bin";
    let data = vec![21u8; 4 * 1024 * 1024];
    let source_dir = tempfile::tempdir().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let source_file = source_dir.path().join(name);
    std::fs::write(&source_file, &data).unwrap();
    let magic_ipv4_addr = ephemeral_loopback_addr();

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let sender = runtime
        .block_on(sendmer::send_handle(
            source_file,
            sendmer::SendOptions {
                relay_mode: sendmer::RelayModeOption::Disabled,
                ticket_type: sendmer::AddrInfoOptions::Addresses,
                magic_ipv4_addr: Some(magic_ipv4_addr),
                max_upload_rate_bytes_per_sec: std::num::NonZeroU64::new(128 * 1024),
                ..Default::default()
            },
        ))
        .expect("library sender should start");
    let ticket = sender.ticket().to_string();

    let mut receiver = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--magic-ipv4-addr",
            "127.0.0.1:0",
            "--json-events",
            "--retry-limit",
            "1",
            "--retry-backoff-ms",
            "0",
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            &ticket,
        ])
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI receiver should start");
    let stdout = receiver.stdout.take().expect("receiver stdout");
    let (line_sender, line_receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line_sender.send(line).is_err() => break,
                Ok(_) => {}
            }
        }
    });

    let (mut events, reader) =
        wait_for_cancellation_progress(&mut receiver, &line_receiver, reader);

    let cancel_result = runtime.block_on(sender.cancel());
    if let Err(error) = cancel_result {
        let _ = receiver.kill();
        let _ = receiver.wait();
        panic!("sender cancellation should close resources cleanly: {error:#}");
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    let receiver_status = loop {
        if let Some(status) = receiver.try_wait().expect("check receiver status") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = receiver.kill();
            let _ = receiver.wait();
            panic!("receiver did not stop after sender cancellation");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    reader.join().expect("join receiver event reader");

    assert!(
        !receiver_status.success(),
        "receiver must fail after the sender is cancelled"
    );
    for line in line_receiver.try_iter() {
        events.push(
            serde_json::from_str::<TransferEvent>(line.trim())
                .expect("receiver event after cancellation should be valid JSON"),
        );
    }
    assert_ordered_single_session(&events);
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Failed { error })
            if error.retryable
                && matches!(
                    error.code,
                    TransferErrorCode::ConnectionFailed | TransferErrorCode::TransferInterrupted
                )
    ));
    assert!(
        !output_dir.path().join(name).exists(),
        "cancelled sender must not leave a partially exported file"
    );
}

#[test]
fn cli_max_receivers_rejects_over_limit_and_releases_slot() {
    let name = "receiver-limit.bin";
    let data = vec![7u8; 2 * 1024 * 1024];
    let source_dir = tempfile::tempdir().unwrap();
    let first_output = tempfile::tempdir().unwrap();
    let rejected_output = tempfile::tempdir().unwrap();
    let reopened_output = tempfile::tempdir().unwrap();
    let source_file = source_dir.path().join(name);
    std::fs::write(&source_file, &data).unwrap();

    let mut sender =
        RunningSend::spawn_with_receiver_limit(&source_file, source_dir.path(), 1, 256 * 1024)
            .expect("limited sender should start");
    let ticket = sender.read_ticket();

    let mut first_receive = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--magic-ipv4-addr",
            "127.0.0.1:0",
            "--json-events",
            "--output-dir",
            first_output.path().to_str().unwrap(),
            &ticket.to_string(),
        ])
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("first receive should start");
    let stdout = first_receive.stdout.take().expect("first receive stdout");
    let (line_sender, line_receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match stdout.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) if line_sender.send(line).is_err() => break,
                Ok(_) => {}
            }
        }
    });

    let minimum_progress = 256 * 1024;
    loop {
        let line = line_receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("first receive should report progress before the second connection");
        let event: TransferEvent = serde_json::from_str(line.trim()).unwrap_or_else(|error| {
            panic!("first receive emitted invalid JSON event: {error}: {line}")
        });
        match event.event {
            TransferEventData::Progress { processed, .. } if processed >= minimum_progress => {
                break;
            }
            TransferEventData::Failed { error } => {
                panic!("first receive failed before admission check: {error:?}")
            }
            TransferEventData::Completed => {
                panic!("first receive completed before admission check")
            }
            _ => {}
        }
    }

    assert_over_limit_receive_rejected(&ticket, rejected_output.path(), name);

    let first_status = first_receive.wait().expect("wait for first receive");
    reader.join().expect("join first receive event reader");
    assert!(
        first_status.success(),
        "first receive should finish after the rejected connection: {first_status}"
    );
    assert_eq!(std::fs::read(first_output.path().join(name)).unwrap(), data);

    let reopened = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--magic-ipv4-addr",
            "127.0.0.1:0",
            "--no-progress",
            "--output-dir",
            reopened_output.path().to_str().unwrap(),
            &ticket.to_string(),
        ])
        .env_remove("RUST_LOG")
        .output()
        .expect("receive after slot release should run");
    sender.cleanup();
    assert!(
        reopened.status.success(),
        "released receiver slot should accept a later receive: {}",
        String::from_utf8_lossy(&reopened.stderr)
    );
    assert_eq!(
        std::fs::read(reopened_output.path().join(name)).unwrap(),
        data
    );
}

fn wait_for_cancellation_progress(
    receiver: &mut Child,
    line_receiver: &std::sync::mpsc::Receiver<String>,
    reader: std::thread::JoinHandle<()>,
) -> (Vec<TransferEvent>, std::thread::JoinHandle<()>) {
    // Wait for real payload progress before cancelling, proving the receiver had an active
    // provider connection and that cancellation interrupts an in-flight transfer.
    let mut events = Vec::new();
    loop {
        let line = match line_receiver.recv_timeout(Duration::from_secs(30)) {
            Ok(line) => line,
            Err(error) => {
                let status = match receiver
                    .try_wait()
                    .expect("check receiver after progress wait")
                {
                    Some(status) => status,
                    None => {
                        let _ = receiver.kill();
                        receiver
                            .wait()
                            .expect("wait for receiver after progress wait")
                    }
                };
                reader.join().expect("join receiver event reader");
                let stderr_nonempty = receiver.stderr.take().is_some_and(|mut pipe| {
                    let mut stderr = String::new();
                    let _ = pipe.read_to_string(&mut stderr);
                    !stderr.is_empty()
                });
                panic!(
                    "receiver ended before payload progress: channel={error}; status={status};\n\
                     structured events={events:?}; stderr_nonempty={stderr_nonempty}"
                );
            }
        };
        let event: TransferEvent = serde_json::from_str(line.trim()).unwrap_or_else(|error| {
            panic!("receiver emitted invalid JSON before sender cancellation: {error}: {line}")
        });
        let progress_reached = matches!(
            &event.event,
            TransferEventData::Progress { processed, .. } if *processed >= 128 * 1024
        );
        match &event.event {
            TransferEventData::Failed { error } => {
                panic!("receiver failed before sender cancellation: {error:?}")
            }
            TransferEventData::Completed => {
                panic!("receiver completed before sender cancellation")
            }
            _ => {}
        }
        events.push(event);
        if progress_reached {
            break;
        }
    }

    (events, reader)
}

fn assert_over_limit_receive_rejected(ticket: &BlobTicket, rejected_output: &Path, name: &str) {
    let mut rejected_receive = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--magic-ipv4-addr",
            "127.0.0.1:0",
            "--json-events",
            "--retry-limit",
            "1",
            "--output-dir",
            rejected_output.to_str().unwrap(),
            &ticket.to_string(),
        ])
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("second receive should run");
    // Admission rejection produces only metadata events, so its small output fits the pipes.
    // Bound the child lifetime independently of network timeouts and reap it on failure.
    let rejection_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if rejected_receive
            .try_wait()
            .expect("check rejected receive")
            .is_some()
        {
            break;
        }
        if Instant::now() >= rejection_deadline {
            rejected_receive
                .kill()
                .expect("stop stalled rejected receive");
            rejected_receive
                .wait()
                .expect("reap stalled rejected receive");
            panic!("over-limit receive did not exit within 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let rejected = rejected_receive
        .wait_with_output()
        .expect("read rejected receive result");
    assert!(!rejected.status.success(), "over-limit receive must fail");
    let rejected_events = parse_json_events(&rejected.stdout);
    assert_ordered_single_session(&rejected_events);
    assert!(
        matches!(
            rejected_events.last().map(|event| &event.event),
            Some(TransferEventData::Failed { error })
                if error.code == TransferErrorCode::TransferInterrupted
                    && error.phase == sendmer::TransferPhase::Metadata
                    && error.retryable
        ),
        "over-limit receive should expose a retryable metadata failure: {rejected_events:?}"
    );
    assert!(
        !rejected_output.join(name).exists(),
        "rejected receive must not export a partial file"
    );
}
