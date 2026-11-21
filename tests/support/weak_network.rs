//! 在 macOS 回环链路上测试真实 sendmer 进程。测试链路只转发 UDP 数据包。

use super::{read_ticket, sendmer_bin, stop_child};
use anyhow::{Context, ensure};
use iroh::EndpointAddr;
use iroh_blobs::{Hash, ticket::BlobTicket};
use rand::{Rng, SeedableRng, rngs::StdRng};
use sendmer::{TransferEvent, TransferEventData, TransferEventStreamValidator};
use std::{collections::BTreeMap, future::pending, net::SocketAddr, path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, BufReader, Lines},
    net::UdpSocket,
    process::{ChildStdout, Command},
    time::{Duration, Instant, sleep_until, timeout},
};

const PAYLOAD_BYTES: usize = 1024 * 1024;
const RATE_BYTES_PER_SECOND: u64 = 256 * 1024;
const ONE_WAY_MS: u64 = 75;
const JITTER_MS: [u64; 4] = [50, 75, 100, 75];
const OUTAGE: Duration = Duration::from_secs(15);
const SEED: u64 = 21;
const LOOPBACK: &str = "127.0.0.1:0";
const QUEUE_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug)]
enum Network {
    Control,
    RateLimited,
    PacketLoss,
    Outage,
    OutageWithRetry,
    FixedDelay,
    Jitter,
    Combined,
    CombinedWithRetry,
}

impl Network {
    /// 组合用例在断网前后都保持限速、丢包和抖动。
    fn combined(self) -> bool {
        matches!(self, Self::Combined | Self::CombinedWithRetry)
    }

    fn rate_limited(self) -> bool {
        matches!(self, Self::RateLimited) || self.combined()
    }

    fn loses_packets(self) -> bool {
        matches!(self, Self::PacketLoss) || self.combined()
    }

    fn has_outage(self) -> bool {
        matches!(self, Self::Outage | Self::OutageWithRetry) || self.combined()
    }

    fn forces_retry(self) -> bool {
        matches!(self, Self::OutageWithRetry | Self::CombinedWithRetry)
    }

    /// 使用不同的包序号偏移，避免两个方向总在相同位置抖动。
    fn delay(self, sequence: u64) -> Duration {
        Duration::from_millis(match self {
            Self::FixedDelay => ONE_WAY_MS,
            Self::Jitter | Self::Combined | Self::CombinedWithRetry => {
                JITTER_MS[sequence as usize % JITTER_MS.len()]
            }
            _ => 0,
        })
    }
}

#[derive(Default)]
struct Direction {
    eligible: u64,
    forwarded_bytes: u64,
    lost: u64,
    lost_after_outage: u64,
    blocked: u64,
    min_wait: Option<Duration>,
    max_wait: Duration,
}

impl Direction {
    /// 保存实际排队时间的上下界。组合用例的下行时间包括限速等待。
    fn record_wait(&mut self, elapsed: Duration) {
        self.min_wait = Some(self.min_wait.map_or(elapsed, |min| min.min(elapsed)));
        self.max_wait = self.max_wait.max(elapsed);
    }
}

#[derive(Default)]
struct Traffic {
    uplink: Direction,
    downlink: Direction,
    outage_start: Option<Instant>,
    first_packet_after_outage: Option<Instant>,
    downlink_before_outage: u64,
}

struct QueuedPacket {
    to: SocketAddr,
    downlink: bool,
    arrived: Instant,
    data: Vec<u8>,
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
/// 按到期时间转发，允许抖动改变包的先后顺序。队列满时测试失败。
async fn forward_udp(
    socket: &UdpSocket,
    sender: SocketAddr,
    network: Network,
    output: &Path,
    traffic: &mut Traffic,
) -> anyhow::Result<()> {
    let mut receiver = None;
    let mut buffer = vec![0; 65_536];
    let mut queue: BTreeMap<(Instant, u64), QueuedPacket> = BTreeMap::new();
    let mut next_downlink = Instant::now();
    let mut sequence = 0;

    loop {
        let next_due = queue.first_key_value().map(|(&(due, _), _)| due);
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
                if traffic.outage_start.is_some_and(|start| start.elapsed() < OUTAGE) {
                    direction.blocked += 1;
                    continue;
                }
                // 只对断网时段之外的数据包施加 3% 丢包，两个方向错开位置。
                direction.eligible += 1;
                let offset = if downlink { SEED } else { SEED + 7 };
                let link_sequence = direction.eligible + offset;
                if network.loses_packets() && matches!(link_sequence % 100, 11 | 47 | 83) {
                    direction.lost += 1;
                    direction.lost_after_outage += u64::from(traffic.outage_start.is_some());
                    continue;
                }

                let arrived = Instant::now();
                // 下行包先按包长占用带宽，再叠加单程延迟。
                let send_at = if downlink && network.rate_limited() {
                    let send_time = Duration::from_nanos(
                        len as u64 * 1_000_000_000 / RATE_BYTES_PER_SECOND,
                    );
                    next_downlink = next_downlink.max(arrived) + send_time;
                    next_downlink
                } else {
                    arrived
                };
                let due = send_at + network.delay(link_sequence);
                ensure!(queue.len() < QUEUE_LIMIT, "UDP forwarding queue overflowed");
                sequence += 1;
                queue.insert((due, sequence), QueuedPacket {
                    to,
                    downlink,
                    arrived,
                    data: buffer[..len].to_vec(),
                });
            }
            () = async {
                match next_due {
                    Some(due) if due > Instant::now() => sleep_until(due).await,
                    Some(_) => {}
                    None => pending().await,
                }
            } => {
                let (_, packet) = queue.pop_first().expect("转发队列已有到期数据包");
                let direction = if packet.downlink {
                    &mut traffic.downlink
                } else {
                    &mut traffic.uplink
                };
                let first_after_outage =
                    traffic.outage_start.is_some() && traffic.first_packet_after_outage.is_none();
                if first_after_outage {
                    ensure!(
                        std::fs::read_dir(output)?.next().is_none(),
                        "output was visible before traffic resumed"
                    );
                }
                direction.forwarded_bytes += socket.send_to(&packet.data, packet.to).await? as u64;
                let forwarded = Instant::now();
                direction.record_wait(forwarded - packet.arrived);
                if first_after_outage {
                    traffic.first_packet_after_outage = Some(forwarded);
                }
                if packet.downlink && network.has_outage()
                    && traffic.outage_start.is_none()
                    && direction.forwarded_bytes >= PAYLOAD_BYTES as u64 / 2
                {
                    traffic.downlink_before_outage = direction.forwarded_bytes;
                    traffic.outage_start = Some(Instant::now());
                    // 断网丢弃尚未转发的包。恢复后只处理端点重新发出的包。
                    for queued in std::mem::take(&mut queue).into_values() {
                        if queued.downlink {
                            traffic.downlink.blocked += 1;
                        } else {
                            traffic.uplink.blocked += 1;
                        }
                    }
                    next_downlink = Instant::now();
                }
            }
        }
    }
}

/// 读完进程发出的事件。验证顺序、单一终态及重连后的累计进度。
async fn verify_events(lines: &mut Lines<BufReader<ChildStdout>>) -> anyhow::Result<Instant> {
    let mut validator = TransferEventStreamValidator::new();
    let mut completed = None;
    let mut progress = 0;
    while let Some(line) = lines.next_line().await? {
        let event: TransferEvent = serde_json::from_str(&line)?;
        validator.accept(&event)?;
        match event.event {
            TransferEventData::Progress { processed, .. } => {
                ensure!(processed >= progress, "receive progress regressed");
                progress = processed;
            }
            TransferEventData::Completed => completed = Some(Instant::now()),
            TransferEventData::Failed { error } => anyhow::bail!("receive failed: {error:?}"),
            _ => {}
        }
    }
    completed.context("receiver did not report completion")
}

/// 核对实际转发量和故障计数。断网后转发的仍是加密 UDP 字节，包含协议开销。
fn verify_link(
    network: Network,
    traffic: &Traffic,
    started: Instant,
    completed: Instant,
    retry_count: usize,
) -> anyhow::Result<()> {
    let downlink = &traffic.downlink;
    let uplink = &traffic.uplink;
    let elapsed = completed - started;
    eprintln!(
        "{network:?}: seed={SEED} elapsed_ms={} downlink_bytes={} uplink_bytes={} loss_down={}/{} loss_up={}/{} outage_drop_down={} outage_drop_up={} retries={retry_count}",
        elapsed.as_millis(),
        downlink.forwarded_bytes,
        uplink.forwarded_bytes,
        downlink.lost,
        downlink.eligible,
        uplink.lost,
        uplink.eligible,
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
    if network.rate_limited() {
        let active = if network.has_outage() {
            elapsed.saturating_sub(OUTAGE)
        } else {
            elapsed
        };
        let nominal = Duration::from_secs(PAYLOAD_BYTES as u64 / RATE_BYTES_PER_SECOND);
        ensure!(
            active >= nominal - Duration::from_millis(100),
            "transfer completed faster than the configured link rate"
        );
    }
    if network.loses_packets() {
        ensure!(
            downlink.lost > 0 && uplink.lost > 0,
            "bidirectional loss was not exercised"
        );
    }
    verify_delay(network, downlink, uplink)?;
    if network.has_outage() {
        let outage_end = traffic
            .outage_start
            .context("test did not start the outage")?
            + OUTAGE;
        let first = traffic
            .first_packet_after_outage
            .context("traffic did not resume")?;
        ensure!(
            first >= outage_end,
            "traffic passed before the outage ended"
        );
        let resumed_bytes = downlink.forwarded_bytes - traffic.downlink_before_outage;
        eprintln!(
            "{network:?}: after_outage_first_packet_ms={} after_outage_completed_ms={} downlink_before={} downlink_after={resumed_bytes}",
            (first - outage_end).as_millis(),
            (completed - outage_end).as_millis(),
            traffic.downlink_before_outage,
        );
        ensure!(
            downlink.blocked > 0 && uplink.blocked > 0,
            "bidirectional traffic did not encounter the outage"
        );
        if network.combined() {
            eprintln!(
                "{network:?}: after_outage_loss_down={} after_outage_loss_up={}",
                downlink.lost_after_outage, uplink.lost_after_outage
            );
            ensure!(
                downlink.lost_after_outage > 0 && uplink.lost_after_outage > 0,
                "bidirectional loss ended with the outage"
            );
        }
        if network.forces_retry() {
            ensure!(retry_count > 0, "receiver did not execute a download retry");
        }
        if !network.combined() {
            ensure!(
                resumed_bytes < PAYLOAD_BYTES as u64 * 3 / 4,
                "download resent too much data after reconnection: {resumed_bytes} bytes"
            );
        }
    }
    Ok(())
}

/// 组合弱网会增加 QUIC 重传。用 iroh-blobs 的成功请求统计量核对重试实际读取的载荷。
fn verify_combined_payload(network: Network, log: &str, retry_count: usize) -> anyhow::Result<()> {
    let payload = log
        .lines()
        .rev()
        .find(|line| line.contains("get hash seq done"))
        .and_then(|line| line.split_once("payload_bytes_read: "))
        .and_then(|(_, counters)| counters.split_once(','))
        .context("missing completed iroh-blobs request counters")?
        .0
        .parse::<u64>()?;
    eprintln!("{network:?}: successful_request_payload_bytes={payload}");
    if retry_count > 0 {
        ensure!(
            (1..PAYLOAD_BYTES as u64).contains(&payload),
            "successful retry did not reuse verified data: {payload} payload bytes"
        );
    } else {
        ensure!(
            payload >= PAYLOAD_BYTES as u64,
            "the uninterrupted download request did not account for the complete payload"
        );
    }
    Ok(())
}

/// 核对两个方向的实测排队时间，确认延迟下限及抖动跨度确实施加。
fn verify_delay(network: Network, downlink: &Direction, uplink: &Direction) -> anyhow::Result<()> {
    let fixed = matches!(network, Network::FixedDelay);
    if fixed || matches!(network, Network::Jitter) || network.combined() {
        for (name, direction) in [("downlink", downlink), ("uplink", uplink)] {
            let min = direction.min_wait.context("no forwarded delay samples")?;
            let max = direction.max_wait;
            eprintln!(
                "{network:?}: {name}_queue_ms={}..{}",
                min.as_millis(),
                max.as_millis()
            );
            if fixed {
                ensure!(
                    min >= Duration::from_millis(ONE_WAY_MS),
                    "{name} delay was too short"
                );
            } else {
                let low = Duration::from_millis(JITTER_MS[0]);
                let high = Duration::from_millis(JITTER_MS[2]);
                ensure!(
                    min >= low && max >= high && max - min >= (high - low) / 2,
                    "{name} jitter did not exercise the configured delay range"
                );
            }
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
