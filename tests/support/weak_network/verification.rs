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
