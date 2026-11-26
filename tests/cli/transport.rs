use super::{
    RunningSend, assert_ordered_single_session, list_receive_temp_dirs, parse_json_events,
    sendmer_bin,
};
use iroh::EndpointAddr;
use iroh_blobs::ticket::BlobTicket;
use iroh_blobs::{BlobFormat, Hash};
use sendmer::TransferEventData;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
#[test]
fn send_recv_file() {
    let name = "somefile.bin";
    let data = vec![0u8; 100];
    // create src and tgt dir, and src file
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();
    let mut send = RunningSend::spawn(&src_file, src_dir.path()).unwrap();
    let ticket = send.read_ticket();
    // Call library `download` directly to keep tests focused on library API.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let opts = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: Default::default(),
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let res = rt
        .block_on(async { sendmer::receive_with_events(ticket.to_string(), opts, None).await })
        .unwrap();
    send.cleanup();
    assert!(res.message.contains("Downloaded"));
    let expected_root = tgt_dir.path().join(name);
    assert_eq!(res.file_path, expected_root);
    let tgt_file = tgt_dir.path().join(name);
    let tgt_data = std::fs::read(tgt_file).unwrap();
    assert_eq!(tgt_data, data);
}

#[test]
fn send_upload_rate_caps_local_payload_transfer() {
    let name = "rate-limited.bin";
    let data = vec![9u8; 256 * 1024];
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();

    let mut send =
        RunningSend::spawn_with_upload_rate(&src_file, src_dir.path(), Some(64 * 1024)).unwrap();
    let ticket = send.read_ticket();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let options = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: Default::default(),
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };

    let started = Instant::now();
    runtime
        .block_on(sendmer::receive_with_events(
            ticket.to_string(),
            options,
            None,
        ))
        .expect("rate-limited receive should succeed");
    let elapsed = started.elapsed();
    send.cleanup();

    assert_eq!(std::fs::read(tgt_dir.path().join(name)).unwrap(), data);
    assert!(
        elapsed >= Duration::from_secs(2),
        "256 KiB at a 64 KiB/s payload cap completed too quickly: {elapsed:?}"
    );
}

#[test]
fn send_upload_rate_is_shared_by_parallel_receivers() {
    let name = "parallel-rate-limited.bin";
    let data = vec![7u8; 128 * 1024];
    let src_dir = tempfile::tempdir().unwrap();
    let first_target = tempfile::tempdir().unwrap();
    let second_target = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();

    let mut send =
        RunningSend::spawn_with_upload_rate(&src_file, src_dir.path(), Some(32 * 1024)).unwrap();
    let ticket = send.read_ticket();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let receive_options = |output_dir: &Path| sendmer::ReceiveOptions {
        output_dir: Some(output_dir.to_path_buf()),
        relay_mode: sendmer::RelayModeOption::Disabled,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };

    let started = Instant::now();
    let (first, second) = runtime.block_on(async {
        tokio::join!(
            sendmer::receive_with_events(
                ticket.to_string(),
                receive_options(first_target.path()),
                None,
            ),
            sendmer::receive_with_events(
                ticket.to_string(),
                receive_options(second_target.path()),
                None,
            ),
        )
    });
    let elapsed = started.elapsed();
    send.cleanup();

    first.expect("first parallel receive should succeed");
    second.expect("second parallel receive should succeed");
    assert_eq!(std::fs::read(first_target.path().join(name)).unwrap(), data);
    assert_eq!(
        std::fs::read(second_target.path().join(name)).unwrap(),
        data
    );
    assert!(
        elapsed >= Duration::from_secs(4),
        "parallel receivers completed too quickly for a shared 32 KiB/s cap: {elapsed:?}"
    );
}

#[test]
fn send_recv_dir() {
    fn create_file(base: &Path, i: usize, j: usize, k: usize) -> (PathBuf, Vec<u8>) {
        let name = base
            .join(format!("dir-{i}"))
            .join(format!("subdir-{j}"))
            .join(format!("file-{k}"));
        let len = i * 100 + j * 10 + k;
        let data = vec![0u8; len];
        (name, data)
    }

    // create src and tgt dir, and src file
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let src_data_dir = src_dir.path().join("data");
    let tgt_data_dir = tgt_dir.path().join("data");
    // create a complex directory structure
    for i in 0..5 {
        for j in 0..5 {
            for k in 0..5 {
                let (name, data) = create_file(&src_data_dir, i, j, k);
                std::fs::create_dir_all(name.parent().unwrap()).unwrap();
                std::fs::write(&name, &data).unwrap();
            }
        }
    }
    let mut send = RunningSend::spawn(&src_data_dir, src_dir.path()).unwrap();
    let ticket = send.read_ticket();
    // Call library `download` directly to keep tests focused on library API.
    let rt = tokio::runtime::Runtime::new().unwrap();
    let opts = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: Default::default(),
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let res = rt
        .block_on(async { sendmer::receive_with_events(ticket.to_string(), opts, None).await })
        .unwrap();
    send.cleanup();
    assert!(res.message.contains("Downloaded"));
    let expected_root = tgt_data_dir.clone();
    assert_eq!(res.file_path, expected_root);
    // validate directory structure
    for i in 0..5 {
        for j in 0..5 {
            for k in 0..5 {
                let (name, data) = create_file(&tgt_data_dir, i, j, k);
                let tgt_data = std::fs::read(&name).unwrap();
                assert_eq!(tgt_data, data);
            }
        }
    }
}

#[test]
fn receive_fails_on_existing_target_and_cleans_temp_dir() {
    let name = "collision.bin";
    let data = vec![1u8; 64];
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();

    // Pre-create a conflicting target file so export must fail.
    std::fs::write(tgt_dir.path().join(name), b"existing").unwrap();

    let before = list_receive_temp_dirs();

    let mut send = RunningSend::spawn(&src_file, src_dir.path()).unwrap();
    let ticket = send.read_ticket();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let opts = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: Default::default(),
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let err = rt
        .block_on(async { sendmer::receive_with_events(ticket.to_string(), opts, None).await })
        .expect_err("receive should fail when target file already exists");
    send.cleanup();

    assert!(err.to_string().contains("already exists"));

    let after = list_receive_temp_dirs();
    let leaked = after
        .difference(&before)
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(&ticket.hash().to_hex()))
        })
        .collect::<Vec<_>>();
    assert!(
        leaked.is_empty(),
        "temporary receive dirs should be cleaned: {leaked:?}"
    );
}

#[test]
fn receive_fails_on_existing_directory_and_preserves_contents() {
    let name = "directory-collision";
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let src_root = src_dir.path().join(name);
    std::fs::create_dir_all(&src_root).unwrap();
    std::fs::write(src_root.join("incoming.txt"), b"incoming").unwrap();

    let existing = tgt_dir.path().join(name);
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("keep.txt"), b"existing").unwrap();

    let before = list_receive_temp_dirs();
    let mut send = RunningSend::spawn(&src_root, src_dir.path()).unwrap();
    let ticket = send.read_ticket();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let opts = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: Default::default(),
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let error = rt
        .block_on(async { sendmer::receive_with_events(ticket.to_string(), opts, None).await })
        .expect_err("receive should fail when target directory already exists");
    send.cleanup();

    assert!(error.to_string().contains("already exists"));
    assert_eq!(
        std::fs::read(existing.join("keep.txt")).unwrap(),
        b"existing"
    );
    assert!(
        !existing.join("incoming.txt").exists(),
        "incoming data must not merge into the existing directory"
    );

    let after = list_receive_temp_dirs();
    let leaked = after
        .difference(&before)
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(&ticket.hash().to_hex()))
        })
        .collect::<Vec<_>>();
    assert!(
        leaked.is_empty(),
        "temporary receive dirs should be cleaned: {leaked:?}"
    );
}

#[test]
fn receive_defaults_to_current_directory_when_output_dir_is_missing() {
    let name = "default-output.bin";
    let data = vec![7u8; 128];

    let src_dir = tempfile::tempdir().unwrap();
    let work_dir = tempfile::tempdir().unwrap();
    let src_file = src_dir.path().join(name);
    std::fs::write(&src_file, &data).unwrap();

    let mut send = RunningSend::spawn(&src_file, src_dir.path()).unwrap();
    let ticket = send.read_ticket();

    let output = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--magic-ipv4-addr",
            "127.0.0.1:0",
            "--json-events",
        ])
        .arg(ticket.to_string())
        .current_dir(work_dir.path())
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("receive without an output directory should run in its child working directory");
    send.cleanup();

    assert!(
        output.status.success(),
        "receive should succeed with the child working directory: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Completed)
    ));
    assert_eq!(std::fs::read(work_dir.path().join(name)).unwrap(), data);
}

fn receive_cli_with_relative_output_dir(
    ticket: &BlobTicket,
    work_dir: &Path,
) -> std::process::Output {
    Command::new(sendmer_bin())
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
            "--connect-timeout-ms",
            "5000",
            "--metadata-timeout-ms",
            "5000",
            "--download-idle-timeout-ms",
            "5000",
            "--output-dir",
            "relative-output",
        ])
        .arg(ticket.to_string())
        .current_dir(work_dir)
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("receive CLI with relative output directory should run")
}

fn assert_relative_output_cli_completed(output: &std::process::Output, work_dir: &Path) {
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    let output_dir = work_dir.join("relative-output");
    let staging_dirs = std::fs::read_dir(&output_dir)
        .expect("receive creates the relative output directory")
        .map(|entry| entry.expect("output directory entry").path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".sendmer-stage-"))
        })
        .collect::<Vec<_>>();
    assert!(
        staging_dirs.is_empty(),
        "staging directories leaked: {staging_dirs:?}"
    );
    assert!(
        output.status.success(),
        "relative output directory receive should succeed; events={events:?}; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Completed)
    ));
}

#[test]
fn receive_cli_legacy_file_supports_relative_output_dir() {
    let name = "relative-output-legacy.bin";
    let data = b"legacy relative output";
    let source_dir = tempfile::tempdir().unwrap();
    let receiver_cwd = tempfile::tempdir().unwrap();
    let source_file = source_dir.path().join(name);
    std::fs::write(&source_file, data).unwrap();

    let mut sender = RunningSend::spawn(&source_file, source_dir.path()).unwrap();
    let ticket = sender.read_ticket();
    let output = receive_cli_with_relative_output_dir(&ticket, receiver_cwd.path());
    sender.cleanup();

    assert_relative_output_cli_completed(&output, receiver_cwd.path());
    assert_eq!(
        std::fs::read(receiver_cwd.path().join("relative-output").join(name)).unwrap(),
        data
    );
}

/// 每次成功接收只释放该接收连接。同一票据仍可占用空出的名额。
#[test]
fn cli_ticket_survives_two_sequential_receives() {
    let name = "shared-twice.bin";
    let data = b"the original ticket serves both receivers";
    let source_dir = tempfile::tempdir().unwrap();
    let source_file = source_dir.path().join(name);
    std::fs::write(&source_file, data).unwrap();
    let mut sender =
        RunningSend::spawn_with_receiver_limit(&source_file, source_dir.path(), 1, 256 * 1024)
            .unwrap();
    let ticket = sender.read_ticket();

    for receive_index in 0..2 {
        let receiver_cwd = tempfile::tempdir().unwrap();
        let output = receive_cli_with_relative_output_dir(&ticket, receiver_cwd.path());
        assert_relative_output_cli_completed(&output, receiver_cwd.path());
        assert_eq!(
            std::fs::read(receiver_cwd.path().join("relative-output").join(name)).unwrap(),
            data
        );
        if receive_index == 0 {
            // 等过发送端的 500 ms 请求汇总时间，再用原票据接收第二次。
            std::thread::sleep(Duration::from_secs(1));
        }
        assert!(sender.child.try_wait().unwrap().is_none());
    }

    // 发送进程关闭后，同一票据无法再导出文件。
    sender.cleanup();
    let receiver_cwd = tempfile::tempdir().unwrap();
    let output = receive_cli_with_relative_output_dir(&ticket, receiver_cwd.path());
    assert!(!output.status.success());
    let events = parse_json_events(&output.stdout);
    assert_ordered_single_session(&events);
    assert!(matches!(
        events.last().map(|event| &event.event),
        Some(TransferEventData::Failed { .. })
    ));
    assert!(
        !receiver_cwd
            .path()
            .join("relative-output")
            .join(name)
            .exists()
    );
}

#[cfg(feature = "manifest")]
#[test]
fn receive_cli_tm1_directory_supports_relative_output_dir() {
    let source_dir = tempfile::tempdir().unwrap();
    let receiver_cwd = tempfile::tempdir().unwrap();
    let source_root = source_dir.path().join("relative-output-tm1");
    let nested_dir = source_root.join("nested");
    let payload = nested_dir.join("payload.bin");
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::write(&payload, b"TM1 relative output").unwrap();

    let mut sender = RunningSend::spawn_with_manifest(&source_root, source_dir.path()).unwrap();
    let ticket = sender.read_ticket();
    let output = receive_cli_with_relative_output_dir(&ticket, receiver_cwd.path());
    sender.cleanup();

    assert_relative_output_cli_completed(&output, receiver_cwd.path());
    assert_eq!(
        std::fs::read(
            receiver_cwd
                .path()
                .join("relative-output")
                .join("relative-output-tm1/nested/payload.bin")
        )
        .unwrap(),
        b"TM1 relative output"
    );
}

#[test]
fn receive_connection_failure_cleans_temp_dir_after_retries() {
    let output_dir = tempfile::tempdir().unwrap();
    let before = list_receive_temp_dirs();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let secret = sendmer::endpoint_secret().unwrap();
    let ticket = BlobTicket::new(
        EndpointAddr::new(secret.public()),
        Hash::new(b"sendmer-unreachable-retry-test"),
        BlobFormat::HashSeq,
    );
    let result = rt.block_on(async {
        let opts = sendmer::ReceiveOptions {
            output_dir: Some(output_dir.path().to_path_buf()),
            relay_mode: sendmer::RelayModeOption::Disabled,
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
            retry_policy: sendmer::ReceiveRetryPolicy {
                size_fetch_retry_limit: 2,
                size_fetch_chunk_size: 1,
                size_fetch_backoff_ms: 0,
                ..Default::default()
            },
            receive_cache: None,
        };
        sendmer::receive_with_events(ticket.to_string(), opts, None).await
    });
    let error = result.expect_err("self connection should fail immediately");
    assert!(!error.to_string().is_empty());

    let after = list_receive_temp_dirs();
    let leaked = after
        .difference(&before)
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(&ticket.hash().to_hex()))
        })
        .collect::<Vec<_>>();
    assert!(
        leaked.is_empty(),
        "temporary receive dirs should be cleaned after retry failure: {leaked:?}"
    );
}
