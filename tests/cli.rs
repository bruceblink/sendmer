use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
    time::{Duration, Instant},
};

use iroh::EndpointAddr;
use iroh_blobs::{BlobFormat, Hash, ticket::BlobTicket};
use sendmer::{TransferErrorCode, TransferEvent, TransferEventData};

// Resolve the binary before changing the child working directory so parallel
// tests cannot make the command path point at a temporary directory.
fn sendmer_bin() -> PathBuf {
    for variable in ["CARGO_BIN_EXE_sendmer", "CARGO_BIN_EXE_SENDMER"] {
        if let Some(path) = std::env::var_os(variable) {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                return path;
            }
            return Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
        }
    }

    // The integration-test executable lives in target/debug/deps, so its
    // sibling binary is stable even when another test changes process cwd.
    if let Ok(test_executable) = std::env::current_exe()
        && let Some(debug_dir) = test_executable.parent().and_then(Path::parent)
    {
        let mut binary = debug_dir.join("sendmer");
        if cfg!(windows) {
            binary.set_extension("exe");
        }
        if binary.exists() {
            return binary;
        }
    }

    PathBuf::from("sendmer")
}

/// Read `n` lines from `reader`, returning the bytes read including the newlines.
///
/// This assumes that the header lines are ASCII and can be parsed byte by byte.
fn read_ascii_lines(mut n: usize, reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut buf = [0u8; 1];
    let mut res = Vec::new();
    loop {
        if reader.read(&mut buf)? != 1 {
            break;
        }
        let char = buf[0];
        res.push(char);
        if char != b'\n' {
            continue;
        }
        if n > 1 {
            n -= 1;
        } else {
            break;
        }
    }
    Ok(res)
}

fn list_receive_temp_dirs() -> std::collections::HashSet<PathBuf> {
    let prefix = ".sendmer-recv-";
    std::fs::read_dir(std::env::temp_dir())
        .ok()
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix))
        })
        .collect()
}

fn parse_json_events(stdout: &[u8]) -> Vec<TransferEvent> {
    let stdout = std::str::from_utf8(stdout).expect("JSON event stdout should be UTF-8");
    assert!(!stdout.trim().is_empty(), "JSON event stdout was empty");
    stdout
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!(
                    "stdout line {} is not a transfer event: {error}: {line}",
                    index + 1
                )
            })
        })
        .collect()
}

fn assert_ordered_single_session(events: &[TransferEvent]) {
    let first = events.first().expect("started event");
    assert!(matches!(first.event, TransferEventData::Started));
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.schema_version, 1);
        assert_eq!(event.session_id, first.session_id);
        assert_eq!(event.sequence, u64::try_from(index + 1).expect("sequence"));
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.is_terminal())
            .count(),
        1
    );
    assert!(events.last().expect("terminal event").event.is_terminal());
}

#[test]
fn read_ascii_lines_reads_only_requested_lines() {
    let mut input = io::Cursor::new(b"first line\nsecond line\nthird line\n".to_vec());
    let output = read_ascii_lines(2, &mut input).unwrap();
    assert_eq!(
        String::from_utf8(output).unwrap(),
        "first line\nsecond line\n"
    );
}

struct RunningSend {
    child: Child,
}

impl RunningSend {
    fn spawn(path: &Path, cwd: &Path) -> io::Result<Self> {
        Self::spawn_with_upload_rate(path, cwd, None)
    }

    /// Launch a sender with an optional payload cap so integration tests exercise the public CLI.
    fn spawn_with_upload_rate(
        path: &Path,
        cwd: &Path,
        max_upload_rate: Option<u64>,
    ) -> io::Result<Self> {
        let mut command = Command::new(sendmer_bin());
        command
            .args(["send", "--no-progress", "--relay", "disabled"])
            .current_dir(cwd)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(max_upload_rate) = max_upload_rate {
            command
                .arg("--max-upload-rate")
                .arg(max_upload_rate.to_string());
        }
        let child = command.arg(path).spawn()?;
        Ok(Self { child })
    }

    fn read_ticket(&mut self) -> BlobTicket {
        let stdout = self.child.stdout.as_mut().expect("send stdout");
        let mut seen_output = String::new();
        for _ in 0..32 {
            let output = read_ascii_lines(1, stdout).expect("send output line");
            if output.is_empty() {
                let status = self.child.try_wait().expect("send status check");
                let stderr = self.read_stderr();
                panic!(
                    "send exited before printing a valid ticket; status={status:?}, stdout={seen_output:?}, stderr={stderr:?}"
                );
            }
            let output = String::from_utf8(output).expect("utf-8 send output");
            seen_output.push_str(&output);
            if let Some(ticket) = output
                .split_ascii_whitespace()
                .find_map(|token| BlobTicket::from_str(token).ok())
            {
                return ticket;
            }
        }
        let stderr = self.read_stderr();
        panic!("valid ticket not found in send output; stdout={seen_output:?}, stderr={stderr:?}");
    }

    fn cleanup(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn read_stderr(&mut self) -> String {
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        stderr
    }
}

impl Drop for RunningSend {
    fn drop(&mut self) {
        self.cleanup();
    }
}

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
    };
    let res = rt
        .block_on(async { sendmer::receive(ticket.to_string(), opts, None).await })
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
    };

    let started = Instant::now();
    runtime
        .block_on(sendmer::receive(ticket.to_string(), options, None))
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
    };
    let res = rt
        .block_on(async { sendmer::receive(ticket.to_string(), opts, None).await })
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
    };
    let err = rt
        .block_on(async { sendmer::receive(ticket.to_string(), opts, None).await })
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
    };
    let error = rt
        .block_on(async { sendmer::receive(ticket.to_string(), opts, None).await })
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

    let current = std::env::current_dir().unwrap();
    std::env::set_current_dir(work_dir.path()).unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let opts = sendmer::ReceiveOptions {
        output_dir: None,
        relay_mode: Default::default(),
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
    };
    let result = rt.block_on(async { sendmer::receive(ticket.to_string(), opts, None).await });

    std::env::set_current_dir(current).unwrap();
    send.cleanup();

    let res = result.expect("receive should succeed with default output directory");
    assert!(res.message.contains("Downloaded"));

    let received_file = work_dir.path().join(name);
    let received_data = std::fs::read(received_file).unwrap();
    assert_eq!(received_data, data);
}

#[test]
fn receive_connection_failure_cleans_temp_dir_after_retries() {
    let output_dir = tempfile::tempdir().unwrap();
    let before = list_receive_temp_dirs();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let secret = sendmer::core::args::get_or_create_secret().unwrap();
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
            retry_policy: sendmer::core::options::ReceiveRetryPolicy {
                size_fetch_retry_limit: 2,
                size_fetch_chunk_size: 1,
                size_fetch_backoff_ms: 0,
                ..Default::default()
            },
        };
        sendmer::receive(ticket.to_string(), opts, None).await
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

#[test]
fn json_events_keep_piped_stdout_machine_readable_on_receive_failure() {
    let output_dir = tempfile::tempdir().unwrap();
    let secret = sendmer::core::args::get_or_create_secret().unwrap();
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
