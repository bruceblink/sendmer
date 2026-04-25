use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
};

use iroh_blobs::ticket::BlobTicket;

// binary path
fn sendmer_bin() -> String {
    std::env::var("CARGO_BIN_EXE_SENDMER").unwrap_or_else(|_| {
        // 如果环境变量不存在，尝试几种可能的路径
        if cfg!(test) {
            // 在测试环境中，尝试找到构建的二进制
            let mut path = std::env::current_dir().unwrap();
            path.push("target/debug/sendmer");
            if path.exists() {
                return path.to_string_lossy().to_string();
            }
        }
        "sendmer".to_string() // 最后回退到在 PATH 中查找
    })
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
        let child = Command::new(sendmer_bin())
            .args(["send", "--no-progress", path.to_str().unwrap()])
            .current_dir(cwd)
            .env_remove("RUST_LOG")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
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
    };
    let res = rt
        .block_on(async { sendmer::receive(ticket.to_string(), opts, None).await })
        .unwrap();
    send.cleanup();
    assert!(res.message.contains("Downloaded"));
    let tgt_file = tgt_dir.path().join(name);
    let tgt_data = std::fs::read(tgt_file).unwrap();
    assert_eq!(tgt_data, data);
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
    };
    let res = rt
        .block_on(async { sendmer::receive(ticket.to_string(), opts, None).await })
        .unwrap();
    send.cleanup();
    assert!(res.message.contains("Downloaded"));
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
