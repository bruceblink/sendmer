use iroh_blobs::ticket::BlobTicket;
use sendmer::core::options::RelayModeOption;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    str::FromStr,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

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

/// Read the human send output until the relay-only ticket is available.
async fn read_ticket(child: &mut Child) -> anyhow::Result<String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("sender stdout was not piped"))?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    for _ in 0..8 {
        line.clear();
        if reader.read_line(&mut line).await? == 0 {
            anyhow::bail!("sender exited before printing a ticket");
        }
        if let Some(ticket) = line
            .split_ascii_whitespace()
            .find_map(|token| BlobTicket::from_str(token).ok())
        {
            return Ok(ticket.to_string());
        }
    }
    anyhow::bail!("sender output did not contain a valid ticket")
}

async fn stop_child(child: &mut Child) {
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Exercise a relay-only ticket against a real relay selected by the environment.
#[tokio::test]
#[ignore = "requires SENDMER_RELAY_SMOKE=1 and a reachable iroh relay"]
async fn relay_only_ticket_round_trips_a_file() -> anyhow::Result<()> {
    anyhow::ensure!(
        std::env::var("SENDMER_RELAY_SMOKE").as_deref() == Ok("1"),
        "set SENDMER_RELAY_SMOKE=1 to run the opt-in relay smoke"
    );
    let relay = std::env::var("SENDMER_RELAY_URL").unwrap_or_else(|_| "default".to_owned());
    let _relay_mode = RelayModeOption::from_str(&relay)?;
    let source_dir = tempfile::tempdir()?;
    let target_dir = tempfile::tempdir()?;
    let source_file = source_dir.path().join("relay-smoke.bin");
    let data = b"sendmer relay smoke".repeat(4_096);
    tokio::fs::write(&source_file, &data).await?;

    let binary = sendmer_bin();
    let mut sender = Command::new(&binary)
        .args([
            "send",
            "--no-progress",
            "--relay",
            &relay,
            "--ticket-type",
            "relay",
        ])
        .arg(&source_file)
        .env_remove("IROH_SECRET")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let result = async {
        let ticket = tokio::time::timeout(Duration::from_secs(30), read_ticket(&mut sender))
            .await
            .map_err(|_| {
                anyhow::anyhow!("sender did not print a relay ticket within 30 seconds")
            })??;
        let parsed_ticket = BlobTicket::from_str(&ticket)?;
        anyhow::ensure!(
            parsed_ticket.addr().relay_urls().next().is_some(),
            "relay-only smoke ticket did not contain a relay address"
        );
        anyhow::ensure!(
            parsed_ticket.addr().ip_addrs().next().is_none(),
            "relay-only smoke ticket unexpectedly contained an IP address"
        );

        let mut receiver = Command::new(&binary)
            .args([
                "receive",
                &ticket,
                "--output-dir",
                target_dir.path().to_str().expect("utf-8 target path"),
                "--relay",
                &relay,
                "--retry-limit",
                "2",
                "--retry-backoff-ms",
                "250",
                "--connect-timeout-ms",
                "15000",
                "--metadata-timeout-ms",
                "15000",
                "--download-idle-timeout-ms",
                "15000",
                "--no-progress",
            ])
            .env_remove("IROH_SECRET")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        let status = match tokio::time::timeout(Duration::from_secs(45), receiver.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                stop_child(&mut receiver).await;
                anyhow::bail!("relay receive did not finish within 45 seconds")
            }
        };
        anyhow::ensure!(status.success(), "relay receiver exited with {status}");

        let received_path = target_dir.path().join("relay-smoke.bin");
        let received_data = tokio::fs::read(received_path).await?;
        anyhow::ensure!(received_data == data, "relay round-trip payload differs");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    stop_child(&mut sender).await;
    result
}
