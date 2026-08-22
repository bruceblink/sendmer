use std::{
    io::{self, BufRead, BufReader, Read},
    net::UdpSocket,
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

/// Run one manifest-mode sender/receiver round-trip and return the committed destination root.
///
/// This helper deliberately exercises the public CLI and library receive API together, so
/// platform-specific filesystem assertions below cover the same path used by real consumers.
fn round_trip_manifest(source_root: &Path, source_cwd: &Path, output_dir: &Path) -> PathBuf {
    let mut send = RunningSend::spawn_with_manifest(source_root, source_cwd)
        .expect("manifest sender should start");
    let ticket = send.read_ticket();
    let runtime = tokio::runtime::Runtime::new().expect("create receive runtime");
    let options = sendmer::ReceiveOptions {
        output_dir: Some(output_dir.to_path_buf()),
        relay_mode: sendmer::RelayModeOption::Disabled,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let result = runtime
        .block_on(sendmer::receive(ticket.to_string(), options, None))
        .expect("manifest receive should succeed");
    send.cleanup();
    result.file_path
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
        Self::spawn_with_config(path, cwd, max_upload_rate, None, None)
    }

    /// Launch a sender in explicit TM1 manifest mode for v0.10 directory fixtures.
    fn spawn_with_manifest(path: &Path, cwd: &Path) -> io::Result<Self> {
        let child = Command::new(sendmer_bin())
            .args(["send", "--no-progress", "--relay", "disabled", "--manifest"])
            .arg(path)
            .current_dir(cwd)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self { child })
    }

    /// Launch a sender with a stable identity and UDP address so a later process can resume the
    /// same ticket after the original sender is interrupted.
    fn spawn_with_config(
        path: &Path,
        cwd: &Path,
        max_upload_rate: Option<u64>,
        secret: Option<&str>,
        magic_ipv4_addr: Option<&str>,
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
        if let Some(secret) = secret {
            command.env("IROH_SECRET", secret);
        }
        if let Some(magic_ipv4_addr) = magic_ipv4_addr {
            command.args(["--ticket-type", "addresses"]);
            command.arg("--magic-ipv4-addr").arg(magic_ipv4_addr);
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
        receive_cache: None,
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
        receive_cache: None,
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
            sendmer::receive(
                ticket.to_string(),
                receive_options(first_target.path()),
                None,
            ),
            sendmer::receive(
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
fn send_recv_manifest_preserves_empty_directory_and_metadata() {
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("manifest-share");
    let empty = source_root.join("empty").join("nested");
    let payload = source_root.join("readme.txt");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(&payload, b"manifest payload").unwrap();
    let source_time = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&payload, source_time).unwrap();

    let mut send = RunningSend::spawn_with_manifest(&source_root, src_dir.path()).unwrap();
    let ticket = send.read_ticket();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let options = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: sendmer::RelayModeOption::Disabled,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let result = runtime
        .block_on(sendmer::receive(ticket.to_string(), options, None))
        .expect("manifest receive should succeed");
    send.cleanup();

    assert_eq!(result.file_path, tgt_dir.path().join("manifest-share"));
    let received_payload = tgt_dir.path().join("manifest-share/readme.txt");
    assert_eq!(
        std::fs::read(&received_payload).unwrap(),
        b"manifest payload"
    );
    assert_eq!(
        filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(received_payload).unwrap()
        )
        .unix_seconds(),
        source_time.unix_seconds()
    );
    assert!(
        tgt_dir.path().join("manifest-share/empty/nested").is_dir(),
        "manifest mode must materialize empty directories"
    );
}

#[test]
fn send_recv_manifest_preserves_empty_root_directory() {
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("empty-manifest-share");
    std::fs::create_dir_all(&source_root).unwrap();

    let mut send = RunningSend::spawn_with_manifest(&source_root, src_dir.path()).unwrap();
    let ticket = send.read_ticket();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let options = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: sendmer::RelayModeOption::Disabled,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let result = runtime
        .block_on(sendmer::receive(ticket.to_string(), options, None))
        .expect("empty root manifest receive should succeed");
    send.cleanup();

    assert_eq!(
        result.file_path,
        tgt_dir.path().join("empty-manifest-share")
    );
    assert!(result.file_path.is_dir());
}

#[cfg(unix)]
#[test]
fn send_recv_manifest_preserves_posix_mode() {
    use std::os::unix::fs::PermissionsExt;

    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("mode-manifest-share");
    let payload = source_root.join("private.txt");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::write(&payload, b"mode payload").unwrap();
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o640)).unwrap();

    let received_root = round_trip_manifest(&source_root, src_dir.path(), tgt_dir.path());
    let received_mode = std::fs::metadata(received_root.join("private.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(received_mode, 0o640);
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
#[test]
fn send_recv_manifest_preserves_windows_read_only_attribute() {
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("readonly-manifest-share");
    let payload = source_root.join("readonly.txt");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::write(&payload, b"readonly payload").unwrap();
    let mut source_permissions = std::fs::metadata(&payload).unwrap().permissions();
    source_permissions.set_readonly(true);
    std::fs::set_permissions(&payload, source_permissions).unwrap();

    let received_root = round_trip_manifest(&source_root, src_dir.path(), tgt_dir.path());
    let received_payload = received_root.join("readonly.txt");
    let received_readonly = std::fs::metadata(&received_payload)
        .unwrap()
        .permissions()
        .readonly();

    // Restore write access so temporary-directory cleanup is deterministic on Windows.
    let mut target_permissions = std::fs::metadata(&received_payload).unwrap().permissions();
    target_permissions.set_readonly(false);
    std::fs::set_permissions(&received_payload, target_permissions).unwrap();
    let mut source_permissions = std::fs::metadata(&payload).unwrap().permissions();
    source_permissions.set_readonly(false);
    std::fs::set_permissions(&payload, source_permissions).unwrap();

    assert!(
        received_readonly,
        "manifest receive must restore the Windows read-only attribute"
    );
}

// Linux preserves arbitrary Unix filename bytes; macOS filesystems reject this fixture with EILSEQ.
#[cfg(target_os = "linux")]
#[test]
fn send_recv_manifest_round_trips_non_utf8_filename() {
    use std::os::unix::ffi::OsStringExt;

    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("raw-name-manifest-share");
    let raw_name = std::ffi::OsString::from_vec(vec![b'c', 0xff, b'.', b't', b'x', b't']);
    let payload = source_root.join(&raw_name);
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::write(&payload, b"raw name payload").unwrap();

    let received_root = round_trip_manifest(&source_root, src_dir.path(), tgt_dir.path());
    assert_eq!(
        std::fs::read(received_root.join(raw_name)).unwrap(),
        b"raw name payload"
    );
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
        .block_on(sendmer::receive(ticket.to_string(), failed_options, None))
        .expect_err("target collision should preserve the receive cache");

    assert!(cache_entry.join("manifest.json").is_file());
    assert!(cache_entry.join("blobs.db").is_file());
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
        .block_on(sendmer::receive(ticket.to_string(), resumed_options, None))
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
    let data = vec![11u8; 1024 * 1024];
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

    // The progress event precedes the filesystem actor's durable metadata update.
    // Give the rate-limited transfer one short flush window before simulating a crash.
    std::thread::sleep(Duration::from_secs(1));
    first_receive.kill().expect("kill first receive process");
    first_receive.wait().expect("wait for killed receive");
    reader.join().expect("join receive event reader");
    assert!(!output_dir.path().join(name).exists());
    assert!(cache_entry.join("blobs.db").is_file());

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
    let port = UdpSocket::bind(("127.0.0.1", 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let magic_ipv4_addr = format!("127.0.0.1:{port}");
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
    let mut receiver = Command::new(sendmer_bin())
        .args([
            "receive",
            "--relay",
            "disabled",
            "--json-events",
            "--retry-limit",
            "40",
            "--retry-backoff-ms",
            "100",
            "--connect-timeout-ms",
            "500",
            "--metadata-timeout-ms",
            "500",
            "--download-idle-timeout-ms",
            "500",
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

    first_sender.cleanup();
    assert!(cache_entry.join("blobs.db").is_file());

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
        receive_cache: None,
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
            receive_cache: None,
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
        .block_on(sendmer::receive(ticket.to_string(), options, None))
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
                && error.phase == sendmer::core::events::TransferPhase::Preparing
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
                && error.phase == sendmer::core::events::TransferPhase::Preparing
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
