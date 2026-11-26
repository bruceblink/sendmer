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
