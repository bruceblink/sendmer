//! 在 macOS 和 Windows 回环链路上测试真实 sendmer 进程。测试链路只转发 UDP 数据包。

#[path = "weak_network/forwarding.rs"]
mod forwarding;
#[path = "weak_network/verification.rs"]
mod verification;

use super::{read_ticket, sendmer_bin, stop_child};
use anyhow::{Context, ensure};
use forwarding::{LOOPBACK, Network, PAYLOAD_BYTES, SEED, Traffic, forward_udp};
use iroh::EndpointAddr;
use iroh_blobs::Hash;
use iroh_blobs::ticket::BlobTicket;
use rand::{Rng, SeedableRng, rngs::StdRng};
use std::path::Path;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UdpSocket;
use tokio::process::Command;
use tokio::time::{Duration, Instant, timeout};
use verification::{verify_combined_payload, verify_events, verify_link};

/// 在 macOS 上限制子进程只能向转发端口发包，防止 iroh 建连后改走直连。
#[cfg(target_os = "macos")]
fn isolated_sendmer(proxy_port: u16) -> Command {
    let mut command = Command::new("/usr/bin/sandbox-exec");
    let profile = format!(
        r#"(version 1)
           (allow default)
           (deny network-outbound)
           (allow network-outbound (remote ip "localhost:{proxy_port}"))"#
    );
    command.args(["-p", &profile]).arg(sendmer_bin());
    command
}

/// 每一组使用独立进程和缓存。TM1 用例同时检查目录内容和空子目录。
async fn run_case(network: Network, manifest: bool) -> anyhow::Result<()> {
    let temp = tempfile::tempdir()?;
    let source = temp.path().join("source");
    let output = temp.path().join("output");
    let cache = temp.path().join("cache");
    let mut data = vec![0; PAYLOAD_BYTES];
    StdRng::seed_from_u64(SEED).fill(&mut data[..]);
    let name = "payload.bin";
    let input = if manifest {
        tokio::fs::create_dir_all(source.join("empty")).await?;
        tokio::fs::write(source.join(name), &data).await?;
        source
    } else {
        let file = temp.path().join(name);
        tokio::fs::write(&file, &data).await?;
        file
    };
    tokio::fs::create_dir(&output).await?;
    let link = UdpSocket::bind(LOOPBACK).await?;
    let mut send = isolated_sendmer(link.local_addr()?.port());
    send.args([
        "send",
        "--no-progress",
        "--relay",
        "disabled",
        "--ticket-type",
        "addresses",
        "--magic-ipv4-addr",
        LOOPBACK,
    ]);
    if manifest {
        send.arg("--manifest");
    }
    let mut sender = send
        .arg(&input)
        .env("TMPDIR", temp.path())
        .env_remove("IROH_SECRET")
        .env_remove("RUST_LOG")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;

    let result = receive_through_link(&mut sender, &link, &output, &cache, network).await;
    stop_child(&mut sender).await;
    result?;

    let actual = if manifest {
        ensure!(
            output.join("source/empty").is_dir(),
            "TM1 empty directory was not exported"
        );
        output.join("source").join(name)
    } else {
        output.join(name)
    };
    let expected_hash = Hash::new(&data);
    let actual_hash = Hash::new(tokio::fs::read(actual).await?);
    ensure!(
        actual_hash == expected_hash,
        "received payload checksum differs"
    );
    eprintln!("{network:?}: manifest={manifest} payload_hash={actual_hash}");
    Ok(())
}

/// 票据只携带测试链路地址，且两端均禁用 relay。进程退出后收集重试日志和缓存清理结果。
async fn receive_through_link(
    sender: &mut tokio::process::Child,
    link: &UdpSocket,
    output: &Path,
    cache: &Path,
    network: Network,
) -> anyhow::Result<()> {
    let ticket = timeout(Duration::from_secs(30), read_ticket(sender)).await??;
    let sender_addr = ticket
        .addr()
        .ip_addrs()
        .copied()
        .find(|addr| addr.is_ipv4() && addr.ip().is_loopback())
        .context("sender ticket has no IPv4 loopback address")?;
    let proxy_addr = link.local_addr()?;
    let proxy_ticket = BlobTicket::new(
        EndpointAddr::new(ticket.addr().id).with_ip_addr(proxy_addr),
        ticket.hash(),
        ticket.format(),
    );
    let log_path = cache.with_extension("log");
    let log_file = std::fs::File::create(&log_path)?;
    let started = Instant::now();
    let mut receive = isolated_sendmer(proxy_addr.port());
    receive.args([
        "receive",
        "--json-events",
        "--relay",
        "disabled",
        "--magic-ipv4-addr",
    ]);
    receive.arg(LOOPBACK);
    // 默认参数另有断网用例。这里缩短空闲等待，确保断网期间进入应用层重试。
    if network.forces_retry() {
        receive.args([
            "--retry-limit",
            "20",
            "--retry-backoff-ms",
            "500",
            "--connect-timeout-ms",
            "3000",
            "--download-idle-timeout-ms",
            "2000",
        ]);
    }
    let log_filter = if network.combined() {
        "sendmer=warn,iroh_blobs::api::remote=trace"
    } else {
        "sendmer=warn"
    };
    let mut receiver = receive
        .arg("--output-dir")
        .arg(output)
        .arg("--cache-dir")
        .arg(cache)
        .arg(proxy_ticket.to_string())
        .env_remove("IROH_SECRET")
        .env("RUST_LOG", log_filter)
        .stdout(Stdio::piped())
        .stderr(log_file)
        .kill_on_drop(true)
        .spawn()?;
    let mut lines =
        BufReader::new(receiver.stdout.take().context("missing receive stdout")?).lines();
    let mut traffic = Traffic::default();
    let transfer = async {
        let completed = verify_events(&mut lines).await?;
        let status = receiver.wait().await?;
        ensure!(status.success(), "receiver exited with {status}");
        Ok::<Instant, anyhow::Error>(completed)
    };
    let result = tokio::select! {
        result = timeout(Duration::from_secs(90), transfer) =>
            result.context("receive timed out").and_then(|result| result),
        result = forward_udp(link, sender_addr, network, output, &mut traffic) =>
            result.and_then(|()| anyhow::bail!("UDP forwarder stopped before completion")),
    };
    if result.is_err() {
        stop_child(&mut receiver).await;
    }
    let completed = result?;
    let log = tokio::fs::read_to_string(log_path).await?;
    let retry_count = log.matches("blob download attempt failed").count();
    verify_link(network, &traffic, started, completed, retry_count)?;
    if network.combined() {
        verify_combined_payload(network, &log, retry_count)?;
    }
    let mut entries = tokio::fs::read_dir(cache.join("v1")).await?;
    ensure!(
        entries.next_entry().await?.is_none(),
        "finished receive cache was not removed"
    );
    Ok(())
}

/// 分别验证限速、丢包和双向断网。
#[tokio::test]
#[ignore = "requires SENDMER_WEAK_NETWORK_SMOKE=1; uses only local UDP"]
async fn local_udp_weak_network_baseline() -> anyhow::Result<()> {
    run_cases(&[
        Network::Control,
        Network::RateLimited,
        Network::PacketLoss,
        Network::Outage,
        Network::OutageWithRetry,
    ])
    .await
}

/// 先单独测量固定延迟和抖动，再把抖动、限速、丢包和短时断网叠加。
#[tokio::test]
#[ignore = "requires SENDMER_WEAK_NETWORK_SMOKE=1; uses only local UDP"]
async fn local_udp_latency_and_combined() -> anyhow::Result<()> {
    run_cases(&[
        Network::FixedDelay,
        Network::Jitter,
        Network::Combined,
        Network::CombinedWithRetry,
    ])
    .await
}

/// 所有用例按顺序运行，避免不同用例竞争本机带宽和定时器。
async fn run_cases(networks: &[Network]) -> anyhow::Result<()> {
    ensure!(
        std::env::var("SENDMER_WEAK_NETWORK_SMOKE").as_deref() == Ok("1"),
        "set SENDMER_WEAK_NETWORK_SMOKE=1 to run the local weak-network baseline"
    );
    for manifest in [false, true] {
        for &network in networks {
            run_case(network, manifest)
                .await
                .with_context(|| format!("{network:?}, manifest={manifest}"))?;
        }
    }
    Ok(())
}
