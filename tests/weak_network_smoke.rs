use iroh_blobs::ticket::BlobTicket;
use sendmer::core::{
    events::{TransferEvent, TransferEventData},
    options::RelayModeOption,
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Lines},
    process::{Child, ChildStdout, Command},
};

/// Locate the binary built for this integration test, including Cargo's Windows fallback path.
fn sendmer_bin() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_sendmer") {
        return PathBuf::from(path);
    }
    let mut path = std::env::current_exe()
        .expect("integration test executable path")
        .parent()
        .and_then(Path::parent)
        .expect("debug directory")
        .join("sendmer");
    if cfg!(windows) {
        path.set_extension("exe");
    }
    path
}

/// Read the sender's first human-readable output line and return its ticket.
async fn read_ticket(child: &mut Child) -> anyhow::Result<BlobTicket> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("sender stdout was not piped"))?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    for _ in 0..8 {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            anyhow::bail!("sender exited before printing a ticket")
        }
        if let Some(ticket) = line
            .split_ascii_whitespace()
            .find_map(|token| BlobTicket::from_str(token).ok())
        {
            return Ok(ticket);
        }
    }
    anyhow::bail!("sender output did not contain a valid ticket")
}

/// Stop a child process during cleanup, including when the test timed out.
async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Give both sender processes one identity so the restarted ticket remains addressable.
fn secret_string() -> String {
    let secret = iroh::SecretKey::generate();
    data_encoding::HEXLOWER.encode(&secret.to_bytes())
}

/// Exercise throttled relay delivery, an interrupted sender, and receiver retries.
#[tokio::test]
#[ignore = "requires SENDMER_WEAK_NETWORK_SMOKE=1 and a reachable iroh relay"]
async fn throttled_relay_transfer_recovers_after_sender_restart() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("SENDMER_WEAK_NETWORK_SMOKE").as_deref() == Ok("1"),
        "set SENDMER_WEAK_NETWORK_SMOKE=1 to run the opt-in weak-network smoke"
    );
    let relay = std::env::var("SENDMER_RELAY_URL").unwrap_or_else(|_| "default".to_owned());
    let _relay_mode = RelayModeOption::from_str(&relay)?;
    let source_dir = tempfile::tempdir()?;
    let target_dir = tempfile::tempdir()?;
    let source_file = source_dir.path().join("weak-network.bin");
    let data = b"sendmer weak network recovery".repeat(32_768);
    tokio::fs::write(&source_file, &data).await?;
    let secret = secret_string();
    let binary = sendmer_bin();

    let mut sender = Command::new(&binary)
        .args([
            "send",
            "--no-progress",
            "--relay",
            &relay,
            "--ticket-type",
            "relay",
            "--max-upload-rate",
            "65536",
        ])
        .arg(&source_file)
        .env("IROH_SECRET", &secret)
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let mut receiver = None;
    let mut restarted_sender = None;
    let result = async {
        let ticket = tokio::time::timeout(Duration::from_secs(30), read_ticket(&mut sender))
            .await
            .map_err(|_| anyhow::anyhow!("throttled sender did not print a ticket"))??;
        anyhow::ensure!(
            ticket.addr().relay_urls().next().is_some(),
            "weak-network smoke ticket did not contain a relay address"
        );
        anyhow::ensure!(
            ticket.addr().ip_addrs().next().is_none(),
            "weak-network smoke ticket unexpectedly contained an IP address"
        );

        receiver = Some(
            Command::new(&binary)
                .args([
                    "receive",
                    "--json-events",
                    "--relay",
                    &relay,
                    "--retry-limit",
                    "20",
                    "--retry-backoff-ms",
                    "500",
                    "--connect-timeout-ms",
                    "15000",
                    "--metadata-timeout-ms",
                    "15000",
                    "--download-idle-timeout-ms",
                    "15000",
                    "--output-dir",
                    target_dir.path().to_str().expect("utf-8 target path"),
                ])
                .arg(ticket.to_string())
                .env_remove("IROH_SECRET")
                .env_remove("RUST_LOG")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let receiver_process = receiver
            .as_mut()
            .expect("receiver process was just spawned");
        let stdout = receiver_process
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("receiver stdout was not piped"))?;
        let mut lines = BufReader::new(stdout).lines();
        let target_progress = data.len() as u64 / 4;
        wait_for_progress(&mut lines, target_progress).await?;

        stop_child(&mut sender).await;
        tokio::time::sleep(Duration::from_secs(2)).await;

        restarted_sender = Some(
            Command::new(&binary)
                .args([
                    "send",
                    "--no-progress",
                    "--relay",
                    &relay,
                    "--ticket-type",
                    "relay",
                    "--max-upload-rate",
                    "262144",
                ])
                .arg(&source_file)
                .env("IROH_SECRET", &secret)
                .env_remove("RUST_LOG")
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let restarted_process = restarted_sender
            .as_mut()
            .expect("restarted sender was just spawned");
        let restarted_ticket =
            tokio::time::timeout(Duration::from_secs(30), read_ticket(restarted_process))
                .await
                .map_err(|_| anyhow::anyhow!("restarted sender did not print a ticket"))??;
        anyhow::ensure!(
            restarted_ticket.hash() == ticket.hash()
                && restarted_ticket.addr().id == ticket.addr().id,
            "restarted sender did not preserve the original ticket identity"
        );

        let terminal = tokio::time::timeout(Duration::from_secs(180), async {
            loop {
                let line = lines
                    .next_line()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("receiver exited before a terminal event"))?;
                let event: TransferEvent = serde_json::from_str(&line)?;
                if event.event.is_terminal() {
                    break Ok::<TransferEventData, anyhow::Error>(event.event);
                }
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("receiver did not finish after sender restart"))??;
        anyhow::ensure!(
            matches!(terminal, TransferEventData::Completed),
            "receiver did not complete after retry: {terminal:?}"
        );
        let status = receiver_process.wait().await?;
        anyhow::ensure!(status.success(), "receiver exited with {status}");

        let received = tokio::fs::read(target_dir.path().join("weak-network.bin")).await?;
        anyhow::ensure!(received == data, "weak-network payload differs after retry");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    if let Some(child) = receiver.as_mut() {
        stop_child(child).await;
    }
    if let Some(child) = restarted_sender.as_mut() {
        stop_child(child).await;
    }
    stop_child(&mut sender).await;
    result
}

/// Wait until the receiver has durable-looking progress before interrupting the sender.
async fn wait_for_progress(
    lines: &mut Lines<BufReader<ChildStdout>>,
    target: u64,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let line = tokio::time::timeout(remaining, lines.next_line())
            .await
            .map_err(|_| anyhow::anyhow!("receiver emitted no progress before timeout"))??
            .ok_or_else(|| anyhow::anyhow!("receiver exited before progress"))?;
        let event: TransferEvent = serde_json::from_str(&line)?;
        match event.event {
            TransferEventData::Progress { processed, .. } if processed >= target => return Ok(()),
            TransferEventData::Failed { error } => {
                anyhow::bail!("receiver failed before sender interruption: {error:?}")
            }
            TransferEventData::Completed => {
                anyhow::bail!("receiver completed before sender interruption")
            }
            _ => {}
        }
    }
    anyhow::bail!("receiver did not reach progress target")
}
