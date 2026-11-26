use iroh_blobs::ticket::BlobTicket;
use sendmer::{EventEmitter, TransferEvent, TransferEventData, TransferEventStreamValidator};
use std::io::{self, Read};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::Mutex;

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

/// Select a UDP port on loopback for local sender fixtures, then release it for the sender.
fn ephemeral_loopback_addr() -> SocketAddrV4 {
    let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("reserve loopback UDP port");
    let port = socket.local_addr().expect("read reserved UDP port").port();
    SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)
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

/// Check role consistency and validate the complete public event-stream lifecycle.
fn assert_ordered_single_session(events: &[TransferEvent]) {
    let first = events.first().expect("started event");
    assert!(matches!(first.event, TransferEventData::Started));
    let mut validator = TransferEventStreamValidator::new();
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.schema_version, 1);
        assert_eq!(event.session_id, first.session_id);
        assert_eq!(event.role, first.role);
        assert_eq!(event.sequence, u64::try_from(index + 1).expect("sequence"));
        validator
            .accept(event)
            .unwrap_or_else(|error| panic!("invalid transfer event stream: {error}"));
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event.is_terminal())
            .count(),
        1
    );
    assert!(events.last().expect("terminal event").event.is_terminal());
    assert!(validator.is_terminal());
}

#[derive(Default)]
struct RecordingEmitter {
    events: Mutex<Vec<TransferEvent>>,
}

impl RecordingEmitter {
    fn snapshot(&self) -> Vec<TransferEvent> {
        self.events.lock().expect("event recording lock").clone()
    }
}

impl EventEmitter for RecordingEmitter {
    fn emit(&self, event: &TransferEvent) {
        self.events
            .lock()
            .expect("event recording lock")
            .push(event.clone());
    }
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
        .block_on(sendmer::receive(ticket.to_string(), options))
        .expect("manifest receive should succeed");
    send.cleanup();
    result.file_path
}

impl RunningSend {
    fn spawn(path: &Path, cwd: &Path) -> io::Result<Self> {
        Self::spawn_with_upload_rate(path, cwd, None)
    }

    /// Launch a CLI sender with a direct loopback ticket and an optional payload cap.
    fn spawn_with_upload_rate(
        path: &Path,
        cwd: &Path,
        max_upload_rate: Option<u64>,
    ) -> io::Result<Self> {
        Self::spawn_with_config(path, cwd, max_upload_rate, None, Some("127.0.0.1:0"))
    }

    /// Launch a sender with an active receiver ceiling for cross-process admission tests.
    fn spawn_with_receiver_limit(
        path: &Path,
        cwd: &Path,
        max_receivers: u64,
        max_upload_rate: u64,
    ) -> io::Result<Self> {
        let child = Command::new(sendmer_bin())
            .args([
                "send",
                "--no-progress",
                "--relay",
                "disabled",
                "--ticket-type",
                "addresses",
                "--magic-ipv4-addr",
                "127.0.0.1:0",
            ])
            .arg("--max-receivers")
            .arg(max_receivers.to_string())
            .arg("--max-upload-rate")
            .arg(max_upload_rate.to_string())
            .arg(path)
            .current_dir(cwd)
            .env("TMPDIR", cwd)
            .env("TEMP", cwd)
            .env("TMP", cwd)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self { child })
    }

    /// Launch a sender in explicit TM1 manifest mode for v0.10 directory fixtures.
    fn spawn_with_manifest(path: &Path, cwd: &Path) -> io::Result<Self> {
        let child = Command::new(sendmer_bin())
            .args([
                "send",
                "--no-progress",
                "--relay",
                "disabled",
                "--ticket-type",
                "addresses",
                "--magic-ipv4-addr",
                "127.0.0.1:0",
            ])
            .arg("--manifest")
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

#[path = "cli/cache.rs"]
mod cache;

#[path = "cli/events.rs"]
mod events;

#[path = "cli/lifecycle.rs"]
mod lifecycle;

#[path = "cli/manifest.rs"]
mod manifest;

#[path = "cli/transport.rs"]
mod transport;
