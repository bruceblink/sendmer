use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use console::style;
use futures_buffered::FuturesUnordered;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use iroh_blobs::provider::events::{ProviderMessage, RequestUpdate};
use n0_future::StreamExt;
use tokio::sync::mpsc;
use tracing::log::trace;
use tracing::warn;

#[derive(Debug)]
struct PerConnectionProgress {
    endpoint_id: String,
    requests: BTreeMap<u64, ProgressBar>,
}

async fn per_request_progress(
    mp: MultiProgress,
    connection_id: u64,
    request_id: u64,
    connections: Arc<Mutex<BTreeMap<u64, PerConnectionProgress>>>,
    mut rx: irpc::channel::mpsc::Receiver<RequestUpdate>,
) {
    let pb = mp.add(ProgressBar::hidden());
    let endpoint_id = if let Some(connection) = connections.lock().unwrap().get_mut(&connection_id)
    {
        connection.requests.insert(request_id, pb.clone());
        connection.endpoint_id.clone()
    } else {
        warn!("got request for unknown connection {connection_id}");
        return;
    };
    pb.set_style(
        ProgressStyle::with_template(
            "{msg}{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}",
        ).unwrap()
            .progress_chars("#>-"),
    );
    while let Ok(Some(msg)) = rx.recv().await {
        match msg {
            RequestUpdate::Started(msg) => {
                pb.set_message(format!(
                    "n {} r {}/{} i {} # {}",
                    endpoint_id,
                    connection_id,
                    request_id,
                    msg.index,
                    msg.hash.fmt_short()
                ));
                pb.set_length(msg.size);
            }
            RequestUpdate::Progress(msg) => {
                pb.set_position(msg.end_offset);
            }
            RequestUpdate::Completed(_) => {
                if let Some(msg) = connections.lock().unwrap().get_mut(&connection_id) {
                    msg.requests.remove(&request_id);
                };
            }
            RequestUpdate::Aborted(_) => {
                if let Some(msg) = connections.lock().unwrap().get_mut(&connection_id) {
                    msg.requests.remove(&request_id);
                };
            }
        }
    }
    pb.finish_and_clear();
    mp.remove(&pb);
}

pub async fn show_provide_progress(
    mp: MultiProgress,
    mut recv: mpsc::Receiver<ProviderMessage>,
) -> anyhow::Result<()> {
    let connections = Arc::new(Mutex::new(BTreeMap::new()));
    let mut tasks = FuturesUnordered::new();
    loop {
        tokio::select! {
            biased;
            item = recv.recv() => {
                let Some(item) = item else {
                    break;
                };

                trace!("got event {item:?}");
                match item {
                    ProviderMessage::ClientConnectedNotify(msg) => {
                        let endpoint_id = msg.endpoint_id.map(|id| id.fmt_short().to_string()).unwrap_or_else(|| "?".to_string());
                        let connection_id = msg.connection_id;
                        connections.lock().unwrap().insert(
                            connection_id,
                            PerConnectionProgress {
                                requests: BTreeMap::new(),
                                endpoint_id,
                            },
                        );
                    }
                    ProviderMessage::ConnectionClosed(msg) => {
                        if let Some(connection) = connections.lock().unwrap().remove(&msg.connection_id) {
                            for pb in connection.requests.values() {
                                pb.finish_and_clear();
                                mp.remove(pb);
                            }
                        }
                    }
                    ProviderMessage::GetRequestReceivedNotify(msg) => {
                        let request_id = msg.request_id;
                        let connection_id = msg.connection_id;
                        let connections = connections.clone();
                        let mp = mp.clone();
                        tasks.push(per_request_progress(mp, connection_id, request_id, connections, msg.rx));
                    }
                    _ => {}
                }
            }
            Some(_) = tasks.next(), if !tasks.is_empty() => {}
        }
    }
    while tasks.next().await.is_some() {}
    Ok(())
}

const TICK_MS: u64 = 250;

pub fn make_import_overall_progress() -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.enable_steady_tick(Duration::from_millis(TICK_MS));
    pb.set_style(
        ProgressStyle::with_template(
            "{msg}{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len}",
        )
            .unwrap()
            .progress_chars("#>-"),
    );
    pb
}

pub fn make_import_item_progress() -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.enable_steady_tick(Duration::from_millis(TICK_MS));
    pb.set_style(
        ProgressStyle::with_template("{msg}{spinner:.green} XXXX [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}")
            .unwrap()
            .progress_chars("#>-"),
    );
    pb
}

pub fn make_connect_progress() -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.set_style(
        ProgressStyle::with_template("{prefix}{spinner:.green} Connecting ... [{elapsed_precise}]")
            .unwrap(),
    );
    pb.set_prefix(format!("{} ", style("[1/4]").bold().dim()));
    pb.enable_steady_tick(Duration::from_millis(TICK_MS));
    pb
}

pub fn make_get_sizes_progress() -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.set_style(
        ProgressStyle::with_template(
            "{prefix}{spinner:.green} Getting sizes... [{elapsed_precise}]",
        )
            .unwrap(),
    );
    pb.set_prefix(format!("{} ", style("[2/4]").bold().dim()));
    pb.enable_steady_tick(Duration::from_millis(TICK_MS));
    pb
}

pub fn make_download_progress() -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.enable_steady_tick(Duration::from_millis(TICK_MS));
    pb.set_style(
        ProgressStyle::with_template("{prefix}{spinner:.green}{msg} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} {binary_bytes_per_sec}")
            .unwrap()
            .progress_chars("#>-"),
    );
    pb.set_prefix(format!("{} ", style("[3/4]").bold().dim()));
    pb.set_message("Downloading ...".to_string());
    pb
}

pub fn make_export_overall_progress() -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.enable_steady_tick(Duration::from_millis(TICK_MS));
    pb.set_style(
        ProgressStyle::with_template("{prefix}{msg}{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {human_pos}/{human_len} {per_sec}")
            .unwrap()
            .progress_chars("#>-"),
    );
    pb.set_prefix(format!("{}", style("[4/4]").bold().dim()));
    pb
}

pub fn make_export_item_progress() -> ProgressBar {
    let pb = ProgressBar::hidden();
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(
        ProgressStyle::with_template(
            "{msg}{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes}",
        )
            .unwrap()
            .progress_chars("#>-"),
    );
    pb
}