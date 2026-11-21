//! 在 macOS 回环链路上测试真实 sendmer 进程。测试链路只转发 UDP 数据包。

use super::{read_ticket, sendmer_bin, stop_child};
use anyhow::{Context, ensure};
use iroh::EndpointAddr;
use iroh_blobs::{Hash, ticket::BlobTicket};
use rand::{Rng, SeedableRng, rngs::StdRng};
use sendmer::{TransferEvent, TransferEventData, TransferEventStreamValidator};
use std::{collections::VecDeque, net::SocketAddr, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Lines},
    net::UdpSocket,
    process::{ChildStdout, Command},
    time::{Duration, Instant, sleep_until, timeout},
};

const PAYLOAD_BYTES: usize = 1024 * 1024;
const RATE_BYTES_PER_SECOND: u64 = 256 * 1024;
const OUTAGE: Duration = Duration::from_secs(15);
const SEED: u64 = 21;
const LOOPBACK: &str = "127.0.0.1:0";
const RATE_QUEUE_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug)]
enum Network {
    Control,
    RateLimited,
    PacketLoss,
    Outage,
    OutageWithRetry,
}

#[derive(Default)]
struct Direction {
    seen: u64,
    forwarded_bytes: u64,
    lost: u64,
    blocked: u64,
}

#[derive(Default)]
struct Traffic {
    uplink: Direction,
    downlink: Direction,
    outage_start: Option<Instant>,
    first_packet_after_outage: Option<Instant>,
    downlink_before_outage: u64,
}

/// 限制测试子进程只能向转发端口发包，防止 iroh 建连后改走直连。
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

/// 用同一个本机地址双向转发。发送端地址从票据读取，接收端地址从首个数据包取得。
/// 限速队列有固定上限。队列满时测试失败，避免把测试机上的队列丢包计为配置的丢包。
async fn forward_udp(
    socket: &UdpSocket,
    sender: SocketAddr,
    network: Network,
    output: &Path,
    traffic: &mut Traffic,
) -> anyhow::Result<()> {
    let mut receiver = None;
    let mut buffer = vec![0; 65_536];
    let mut pending: VecDeque<(Vec<u8>, SocketAddr)> = VecDeque::new();
    let mut next_send = Instant::now();

    loop {
        tokio::select! {
            packet = socket.recv_from(&mut buffer) => {
                let (len, from) = packet?;
                let downlink = from == sender;
                let (to, direction) = if downlink {
                    let Some(to) = receiver else { continue };
                    (to, &mut traffic.downlink)
                } else {
                    ensure!(from.ip().is_loopback(), "test packet was not from loopback");
                    ensure!(*receiver.get_or_insert(from) == from, "unexpected receiver address");
                    (sender, &mut traffic.uplink)
                };
                direction.seen += 1;

                if let Some(start) = traffic.outage_start {
                    if start.elapsed() < OUTAGE {
                        direction.blocked += 1;
                        continue;
                    }
                    if traffic.first_packet_after_outage.is_none() {
                        ensure!(
                            std::fs::read_dir(output)?.next().is_none(),
                            "output was visible before the outage ended"
                        );
                        traffic.first_packet_after_outage = Some(Instant::now());
                    }
                }
                // 每个方向每一百个包固定丢三个。错开两个方向的丢包位置。
                let offset = if downlink { SEED } else { SEED + 7 };
                if matches!(network, Network::PacketLoss)
                    && matches!((direction.seen + offset) % 100, 11 | 47 | 83)
                {
                    direction.lost += 1;
                    continue;
                }
                if downlink && matches!(network, Network::RateLimited) {
                    ensure!(pending.len() < RATE_QUEUE_LIMIT, "rate limiter queue overflowed");
                    pending.push_back((buffer[..len].to_vec(), to));
                    continue;
                }
                direction.forwarded_bytes += socket.send_to(&buffer[..len], to).await? as u64;
                if downlink && matches!(network, Network::Outage | Network::OutageWithRetry)
                    && traffic.outage_start.is_none()
                    && direction.forwarded_bytes >= PAYLOAD_BYTES as u64 / 2
                {
                    traffic.downlink_before_outage = direction.forwarded_bytes;
                    traffic.outage_start = Some(Instant::now());
                }
            }
            () = sleep_until(next_send), if !pending.is_empty() => {
                let (packet, to) = pending.pop_front().expect("限速队列已有数据包");
                traffic.downlink.forwarded_bytes += socket.send_to(&packet, to).await? as u64;
                next_send = Instant::now() + Duration::from_nanos(
                    packet.len() as u64 * 1_000_000_000 / RATE_BYTES_PER_SECOND,
                );
            }
        }
    }
}

/// 读完进程发出的事件。验证顺序、单一终态及重连后的累计进度。
async fn verify_events(lines: &mut Lines<BufReader<ChildStdout>>) -> anyhow::Result<()> {
    let mut validator = TransferEventStreamValidator::new();
    let mut completed = false;
    let mut progress = 0;
    while let Some(line) = lines.next_line().await? {
        let event: TransferEvent = serde_json::from_str(&line)?;
        validator.accept(&event)?;
        match event.event {
            TransferEventData::Progress { processed, .. } => {
                ensure!(processed >= progress, "receive progress regressed");
                progress = processed;
            }
            TransferEventData::Completed => completed = true,
            TransferEventData::Failed { error } => anyhow::bail!("receive failed: {error:?}"),
            _ => {}
        }
    }
    ensure!(completed, "receiver did not report completion");
    Ok(())
}

/// 核对实际转发量和故障计数。断网后转发的仍是加密 UDP 字节，包含协议开销。
fn verify_link(
    network: Network,
    traffic: &Traffic,
    elapsed: Duration,
    retry_count: usize,
) -> anyhow::Result<()> {
    let downlink = &traffic.downlink;
    let uplink = &traffic.uplink;
    eprintln!(
        "{network:?}: seed={SEED} elapsed_ms={} downlink_bytes={} uplink_bytes={} loss_down={}/{} loss_up={}/{} outage_drop_down={} outage_drop_up={} retries={retry_count}",
        elapsed.as_millis(),
        downlink.forwarded_bytes,
        uplink.forwarded_bytes,
        downlink.lost,
        downlink.seen,
        uplink.lost,
        uplink.seen,
        downlink.blocked,
        uplink.blocked,
    );
    ensure!(
        uplink.forwarded_bytes > 0,
        "test link did not forward receiver traffic"
    );
    ensure!(
        downlink.forwarded_bytes >= PAYLOAD_BYTES as u64,
        "test link did not carry the complete amount of payload data"
    );
    match network {
        Network::Control => {}
        Network::RateLimited => {
            let nominal = Duration::from_secs(PAYLOAD_BYTES as u64 / RATE_BYTES_PER_SECOND);
            ensure!(
                elapsed >= nominal - Duration::from_millis(100),
                "transfer completed faster than the configured link rate"
            );
        }
        Network::PacketLoss => {
            ensure!(
                downlink.lost > 0 && uplink.lost > 0,
                "bidirectional loss was not exercised"
            );
        }
        Network::Outage | Network::OutageWithRetry => {
            let start = traffic
                .outage_start
                .context("test did not start the outage")?;
            let first = traffic
                .first_packet_after_outage
                .context("traffic did not resume")?;
            let resumed_bytes = downlink.forwarded_bytes - traffic.downlink_before_outage;
            eprintln!(
                "{network:?}: first_packet_ms={} downlink_before={} downlink_after={resumed_bytes}",
                (first - start).as_millis(),
                traffic.downlink_before_outage,
            );
            ensure!(
                first - start >= OUTAGE,
                "traffic passed before the outage ended"
            );
            ensure!(
                downlink.blocked > 0 && uplink.blocked > 0,
                "bidirectional traffic did not encounter the outage"
            );
            if matches!(network, Network::OutageWithRetry) {
                ensure!(retry_count > 0, "receiver did not execute a download retry");
            }
            ensure!(
                resumed_bytes < PAYLOAD_BYTES as u64 * 3 / 4,
                "download resent too much data after reconnection: {resumed_bytes} bytes"
            );
        }
    }
    Ok(())
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
        LOOPBACK,
    ]);
    // 默认参数另有断网用例。这里缩短空闲等待，确保断网期间进入应用层重试。
    if matches!(network, Network::OutageWithRetry) {
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
    let mut receiver = receive
        .arg("--output-dir")
        .arg(output)
        .arg("--cache-dir")
        .arg(cache)
        .arg(proxy_ticket.to_string())
        .env_remove("IROH_SECRET")
        .env("RUST_LOG", "sendmer=warn")
        .stdout(Stdio::piped())
        .stderr(log_file)
        .kill_on_drop(true)
        .spawn()?;
    let mut lines =
        BufReader::new(receiver.stdout.take().context("missing receive stdout")?).lines();
    let mut traffic = Traffic::default();
    let transfer = async {
        verify_events(&mut lines).await?;
        let status = receiver.wait().await?;
        ensure!(status.success(), "receiver exited with {status}");
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::select! {
        result = timeout(Duration::from_secs(90), transfer) =>
            result.context("receive timed out").and_then(|result| result),
        result = forward_udp(link, sender_addr, network, output, &mut traffic) => result,
    };
    if result.is_err() {
        stop_child(&mut receiver).await;
    }
    result?;
    let log = tokio::fs::read_to_string(log_path).await?;
    let retry_count = log.matches("blob download attempt failed").count();
    verify_link(network, &traffic, started.elapsed(), retry_count)?;
    let mut entries = tokio::fs::read_dir(cache.join("v1")).await?;
    ensure!(
        entries.next_entry().await?.is_none(),
        "finished receive cache was not removed"
    );
    Ok(())
}

/// 分别验证速率、丢包和黑洞断网。两个载荷格式共用同一套真实传输流程。
#[tokio::test]
#[ignore = "requires SENDMER_WEAK_NETWORK_SMOKE=1; uses only local UDP"]
async fn local_udp_weak_network_baseline() -> anyhow::Result<()> {
    ensure!(
        std::env::var("SENDMER_WEAK_NETWORK_SMOKE").as_deref() == Ok("1"),
        "set SENDMER_WEAK_NETWORK_SMOKE=1 to run the local weak-network baseline"
    );
    for manifest in [false, true] {
        for network in [
            Network::Control,
            Network::RateLimited,
            Network::PacketLoss,
            Network::Outage,
            Network::OutageWithRetry,
        ] {
            run_case(network, manifest)
                .await
                .with_context(|| format!("{network:?}, manifest={manifest}"))?;
        }
    }
    Ok(())
}
