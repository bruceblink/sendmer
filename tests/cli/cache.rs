use super::{
    RunningSend, ephemeral_loopback_addr,
    recovery::{read_snapshot, wait_for_live_checkpoint},
    sendmer_bin,
};
use iroh::EndpointAddr;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, Hash};
use sendmer::{TransferEvent, TransferEventData};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
#[test]
fn persistent_receive_cache_survives_failure_then_reopens_and_cleans_on_success() {
    let name = "persistent-cache.bin";
    let data = vec![7u8; 128 * 1024];
    let src_dir = tempfile::tempdir().unwrap();
    let first_output = tempfile::tempdir().unwrap();
    let second_output = tempfile::tempdir().unwrap();
    let cache_root = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();
    std::fs::write(first_output.path().join(name), b"existing").unwrap();

    let mut send = RunningSend::spawn(&src_file, src_dir.path()).unwrap();
    let ticket = send.read_ticket();
    let cache_entry = cache_root
        .path()
        .join("v1")
        .join(format!("1-{}", ticket.hash().to_hex()));
    let runtime = tokio::runtime::Runtime::new().unwrap();

    let failed_options = sendmer::ReceiveOptions {
        output_dir: Some(first_output.path().to_path_buf()),
        relay_mode: Default::default(),
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: Some(sendmer::ReceiveCacheOptions::new(cache_root.path())),
    };
    runtime
        .block_on(sendmer::receive_with_events(
            ticket.to_string(),
            failed_options,
            None,
        ))
        .expect_err("target collision should preserve the receive cache");

    assert!(cache_entry.join("manifest.json").is_file());
    assert!(cache_entry.join("blobs.db").is_file());
    let snapshot = read_snapshot(&cache_entry);
    assert_eq!(
        snapshot.last_terminal_state,
        Some(sendmer::RecoveryTerminalState::Failed)
    );
    assert!(snapshot.verified_bytes() > 0);
    assert_eq!(
        std::fs::read(first_output.path().join(name)).unwrap(),
        b"existing"
    );

    let resumed_options = sendmer::ReceiveOptions {
        output_dir: Some(second_output.path().to_path_buf()),
        relay_mode: Default::default(),
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: Some(sendmer::ReceiveCacheOptions::new(cache_root.path())),
    };
    runtime
        .block_on(sendmer::receive_with_events(
            ticket.to_string(),
            resumed_options,
            None,
        ))
        .expect("a later receive should reopen the persisted iroh store");
    send.cleanup();

    assert_eq!(
        std::fs::read(second_output.path().join(name)).unwrap(),
        data
    );
    assert!(
        !cache_entry.exists(),
        "successful receive must delete its completed cache entry"
    );
}

#[test]
fn persistent_receive_cache_recovers_after_receiver_process_is_killed() {
    let name = "cross-process-resume.bin";
    let data = vec![11u8; 2 * 1024 * 1024];
    let src_dir = tempfile::tempdir().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let cache_root = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();

    let mut send =
        RunningSend::spawn_with_upload_rate(&src_file, src_dir.path(), Some(256 * 1024)).unwrap();
    let ticket = send.read_ticket();
    let cache_entry = cache_root
        .path()
        .join("v1")
        .join(format!("1-{}", ticket.hash().to_hex()));

    let mut first_receive = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--magic-ipv4-addr",
            "127.0.0.1:0",
            "--json-events",
            "--cache-dir",
            cache_root.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            &ticket.to_string(),
        ])
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = first_receive.stdout.take().expect("receive stdout");
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

    interrupt_receive_after_progress(&mut first_receive, &line_receiver, reader, &cache_entry);
    assert!(!output_dir.path().join(name).exists());
    assert!(cache_entry.join("blobs.db").is_file());
    let interrupted = read_snapshot(&cache_entry);
    assert!(
        interrupted.can_resume(),
        "crashed receive must persist a verified checkpoint"
    );
    assert_eq!(interrupted.last_terminal_state, None);

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let (durable_bytes, missing_request) = runtime.block_on(async {
        let store = iroh_blobs::store::fs::FsStore::load(&cache_entry)
            .await
            .expect("reopen iroh cache after process crash");
        let local = store
            .remote()
            .local(ticket.hash_and_format())
            .await
            .expect("inspect durable cache ranges");
        let local_bytes = local.local_bytes();
        let missing = local.missing();
        store.shutdown().await.expect("shutdown inspected cache");
        (local_bytes, missing)
    });
    assert!(
        durable_bytes >= 128 * 1024,
        "the killed receive must leave meaningful verified ranges on disk, got {durable_bytes}"
    );
    assert_ne!(
        missing_request,
        iroh_blobs::protocol::GetRequest::all(ticket.hash()),
        "the resumed process must request fewer ranges than a fresh receive"
    );

    let resumed = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--magic-ipv4-addr",
            "127.0.0.1:0",
            "--no-progress",
            "--cache-dir",
            cache_root.path().to_str().unwrap(),
            "--output-dir",
            output_dir.path().to_str().unwrap(),
            &ticket.to_string(),
        ])
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    send.cleanup();

    assert!(
        resumed.status.success(),
        "resumed receive failed: {}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(std::fs::read(output_dir.path().join(name)).unwrap(), data);
    assert!(
        !cache_entry.exists(),
        "completed resumed receive must remove its cache entry"
    );
}

#[test]
fn persistent_receive_cache_recovers_after_sender_restart() {
    let name = "sender-restart.bin";
    let data = vec![29u8; 2 * 1024 * 1024];
    let src_dir = tempfile::tempdir().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let cache_root = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();

    // Reusing both identity and address makes the second sender advertise the original ticket.
    let secret_key = iroh::SecretKey::generate();
    let secret = data_encoding::HEXLOWER.encode(&secret_key.to_bytes());
    let fixed_sender_addr = ephemeral_loopback_addr();
    let port = fixed_sender_addr.port();
    let magic_ipv4_addr = fixed_sender_addr.to_string();
    let mut first_sender = RunningSend::spawn_with_config(
        &src_file,
        src_dir.path(),
        Some(128 * 1024),
        Some(secret.as_str()),
        Some(&magic_ipv4_addr),
    )
    .unwrap();
    let ticket = first_sender.read_ticket();
    assert!(
        ticket.addr().ip_addrs().any(|addr| addr.port() == port),
        "sender ticket should contain the fixed test address: {ticket}"
    );

    let cache_entry = cache_root
        .path()
        .join("v1")
        .join(format!("1-{}", ticket.hash().to_hex()));
    let (mut receiver, line_receiver, reader) =
        spawn_restart_receiver(&ticket, cache_root.path(), output_dir.path());

    let mut progress_values = Vec::new();
    let minimum_progress = 256 * 1024;
    loop {
        let line = line_receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("receive should report progress before sender interruption");
        let event: TransferEvent = serde_json::from_str(line.trim())
            .unwrap_or_else(|error| panic!("receive emitted invalid JSON event: {error}: {line}"));
        match event.event {
            TransferEventData::Progress { processed, .. } => {
                progress_values.push(processed);
                if processed >= minimum_progress {
                    break;
                }
            }
            TransferEventData::Failed { error } => {
                panic!("receive failed before sender interruption: {error:?}")
            }
            TransferEventData::Completed => {
                panic!("receive completed before sender interruption")
            }
            _ => {}
        }
    }

    wait_for_live_checkpoint(&cache_entry, &mut receiver, minimum_progress);
    first_sender.cleanup();
    assert!(cache_entry.join("blobs.db").is_file());
    let before_restart = read_snapshot(&cache_entry);
    assert!(before_restart.can_resume());

    let mut restarted_sender = RunningSend::spawn_with_config(
        &src_file,
        src_dir.path(),
        Some(128 * 1024),
        Some(secret.as_str()),
        Some(&magic_ipv4_addr),
    )
    .expect("restart sender with the original identity and address");
    let restarted_ticket = restarted_sender.read_ticket();
    assert_eq!(restarted_ticket.hash(), ticket.hash());
    assert_eq!(restarted_ticket.addr().id, ticket.addr().id);

    let terminal = loop {
        let line = line_receiver
            .recv_timeout(Duration::from_secs(90))
            .expect("receive should finish after sender restart");
        let event: TransferEvent = serde_json::from_str(line.trim()).unwrap_or_else(|error| {
            panic!("receive emitted invalid JSON event after restart: {error}: {line}")
        });
        if let TransferEventData::Progress { processed, .. } = event.event {
            progress_values.push(processed);
        } else if event.event.is_terminal() {
            break event.event;
        }
    };
    let status = receiver.wait().expect("wait for restarted receive");
    reader.join().expect("join receive event reader");
    restarted_sender.cleanup();

    assert!(
        matches!(terminal, TransferEventData::Completed),
        "sender restart should lead to a completed receive, got {terminal:?}"
    );
    assert!(status.success(), "restarted receive exited with {status}");
    assert_eq!(std::fs::read(output_dir.path().join(name)).unwrap(), data);
    assert!(!cache_entry.exists());
    assert!(
        !progress_values
            .windows(2)
            .any(|values| values[1] < values[0]),
        "progress must stay monotonic across the reconnect: {progress_values:?}"
    );
}

#[test]
fn invalid_receive_cache_ttl_fails_before_creating_cache_root() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let cache_root = temp.path().join("must-not-be-created");
    let output = tempfile::tempdir().unwrap();
    let secret = sendmer::core::args::get_or_create_secret().unwrap();
    let ticket = BlobTicket::new(
        EndpointAddr::new(secret.public()),
        Hash::new(b"invalid receive cache options"),
        BlobFormat::HashSeq,
    );
    let options = sendmer::ReceiveOptions {
        output_dir: Some(output.path().to_path_buf()),
        relay_mode: sendmer::RelayModeOption::Disabled,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: Some(
            sendmer::ReceiveCacheOptions::new(&cache_root).with_ttl(Duration::ZERO),
        ),
    };

    let error = runtime
        .block_on(sendmer::receive_with_events(
            ticket.to_string(),
            options,
            None,
        ))
        .expect_err("zero cache TTL must fail");

    assert!(error.to_string().contains("TTL"));
    assert!(
        !cache_root.exists(),
        "invalid cache settings must fail before creating storage"
    );
}

#[test]
fn cache_prune_cli_removes_expired_entry_and_reports_counts() {
    let cache_root = tempfile::tempdir().unwrap();
    let cache_key = format!("0-{}", "a".repeat(64));
    let entry = cache_root.path().join("v1").join(&cache_key);
    std::fs::create_dir_all(&entry).unwrap();
    std::fs::write(
        entry.join("manifest.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema_version": 1,
            "cache_key": cache_key,
            "created_at_unix_seconds": 0,
            "ttl_seconds": 1
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(entry.join(".lock"), b"0\n").unwrap();

    let output = Command::new(sendmer_bin())
        .args([
            "cache",
            "prune",
            "--cache-dir",
            cache_root.path().to_str().unwrap(),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "cache prune failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Removed 1 expired entries"));
    assert!(!entry.exists());
}

fn interrupt_receive_after_progress(
    first_receive: &mut Child,
    line_receiver: &std::sync::mpsc::Receiver<String>,
    reader: std::thread::JoinHandle<()>,
    cache_entry: &Path,
) {
    let minimum_progress = 256 * 1024;
    loop {
        let line = match line_receiver.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => line,
            Err(error) => {
                let _ = first_receive.kill();
                let _ = first_receive.wait();
                let mut stderr = String::new();
                if let Some(mut pipe) = first_receive.stderr.take() {
                    let _ = pipe.read_to_string(&mut stderr);
                }
                panic!("timed out waiting for partial receive data: {error}; {stderr}");
            }
        };
        let event: TransferEvent = serde_json::from_str(line.trim()).unwrap_or_else(|error| {
            panic!("receive emitted invalid JSON event before interruption: {error}: {line}")
        });
        match event.event {
            TransferEventData::Progress { processed, .. } if processed >= minimum_progress => {
                break;
            }
            TransferEventData::Failed { error } => {
                panic!("receive failed before interruption: {error:?}")
            }
            TransferEventData::Completed => {
                panic!("rate-limited receive completed before it could be interrupted")
            }
            _ => {}
        }
    }

    // Progress is not evidence of a committed checkpoint. Wait for an actual
    // nonempty, active snapshot before interrupting the process.
    wait_for_live_checkpoint(cache_entry, first_receive, minimum_progress);
    first_receive.kill().expect("kill first receive process");
    first_receive.wait().expect("wait for killed receive");
    reader.join().expect("join receive event reader");
}

fn spawn_restart_receiver(
    ticket: &BlobTicket,
    cache_root: &Path,
    output_dir: &Path,
) -> (
    Child,
    std::sync::mpsc::Receiver<String>,
    std::thread::JoinHandle<()>,
) {
    let mut receiver = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--magic-ipv4-addr",
            "127.0.0.1:0",
            "--json-events",
            "--retry-limit",
            "40",
            "--retry-backoff-ms",
            "100",
            "--connect-timeout-ms",
            "5000",
            "--download-idle-timeout-ms",
            "500",
            "--cache-dir",
            cache_root.to_str().unwrap(),
            "--output-dir",
            output_dir.to_str().unwrap(),
            &ticket.to_string(),
        ])
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = receiver.stdout.take().expect("receive stdout");
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

    (receiver, line_receiver, reader)
}
