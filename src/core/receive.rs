//! 接收端功能：根据票据连接远端并导出数据到本地目录。
//!
//! 主要导出 `download`，它负责建立连接、跟踪进度并将文件导出到目标目录。

use crate::core::types::{AppHandle, ReceiveOptions, ReceiveResult, get_or_create_secret};
use crate::core::progress::{emit_event, emit_event_with_payload, emit_progress_event};
use iroh::{Endpoint, discovery::dns::DnsDiscovery};
use iroh_blobs::{
    api::{
        Store,
        blobs::{ExportMode, ExportOptions, ExportProgressItem},
        remote::GetProgressItem,
    },
    format::collection::Collection,
    get::{GetError, Stats, request::get_hash_seq_and_sizes},
    store::fs::FsStore,
    ticket::BlobTicket,
};
use n0_future::StreamExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Instant;
use tokio::select;
use tracing::log::trace;

// event helpers provided by `core::progress`

/// 下载并导出由 `ticket_str` 指定的数据到本地目录。
///
/// - `ticket_str`：连接票据字符串。
/// - `options`：接收选项（输出目录、转发模式等）。
/// - `app_handle`：可选的事件发射器句柄，用于 UI/CLI 上报进度与文件名等信息。
pub async fn download(
    ticket_str: String,
    options: ReceiveOptions,
    app_handle: AppHandle,
) -> anyhow::Result<ReceiveResult> {
    let ticket = BlobTicket::from_str(&ticket_str)?;

    let addr = ticket.addr().clone();

    let secret_key = get_or_create_secret()?;

    let mut builder = Endpoint::builder()
        .alpns(vec![])
        .secret_key(secret_key)
        .relay_mode(options.relay_mode.clone().into());

    if ticket.addr().relay_urls().next().is_none() && ticket.addr().ip_addrs().next().is_none() {
        builder = builder.discovery(DnsDiscovery::n0_dns());
    }
    if let Some(addr) = options.magic_ipv4_addr {
        builder = builder.bind_addr_v4(addr);
    }
    if let Some(addr) = options.magic_ipv6_addr {
        builder = builder.bind_addr_v6(addr);
    }
    let endpoint = builder.bind().await?;

    // Use system temp directory instead of current_dir for GUI app
    // This avoids polluting user directories and OS manages cleanup automatically
    let dir_name = format!(".sendmer-recv-{}", ticket.hash().to_hex());
    let temp_base = std::env::temp_dir();
    let iroh_data_dir = temp_base.join(&dir_name);
    let db = FsStore::load(&iroh_data_dir).await?;
    let db2 = db.clone();
    trace!("load done!");
    let fut = async move {
        let hash_and_format = ticket.hash_and_format();
        let local = db.remote().local(hash_and_format).await?;

        let (stats, total_files, payload_size) = if !local.is_complete() {
            // Emit receive-started event
            emit_event(&app_handle, "receive-started");

            let mut connection = endpoint.connect(addr.clone(), iroh_blobs::protocol::ALPN).await?;

            // Try to get sizes with retries to handle transient connection resets
            let mut sizes_opt: Option<(iroh_blobs::hashseq::HashSeq, std::sync::Arc<[u64]>)> = None;
            let mut last_err: Option<iroh_blobs::get::GetError> = None;
            for attempt in 1..=3 {
                let sizes_result = get_hash_seq_and_sizes(&connection, &hash_and_format.hash, 1024 * 1024 * 32, None).await;
                match sizes_result {
                    Ok((hash_seq, sizes)) => {
                        sizes_opt = Some((hash_seq, sizes));
                        break;
                    }
                    Err(e) => {
                        tracing::error!("Attempt {attempt} to get sizes failed: {e:?}");
                        last_err = Some(e);
                        if attempt < 3 {
                            let backoff = std::time::Duration::from_millis(250 * attempt as u64);
                            tokio::time::sleep(backoff).await;
                            // try to reconnect the endpoint before retrying
                            match endpoint.connect(addr.clone(), iroh_blobs::protocol::ALPN).await {
                                Ok(new_conn) => connection = new_conn,
                                Err(conn_err) => {
                                    tracing::error!("reconnect failed: {conn_err}");
                                }
                            }
                            continue;
                        }
                    }
                }
            }

            let ( _hash_seq, sizes) = match sizes_opt {
                Some((hash_seq, sizes)) => (hash_seq, sizes),
                None => {
                    if let Some(e) = last_err {
                        tracing::error!("Failed to get sizes after retries: {:?}", e);
                        tracing::error!("Error type: {}", std::any::type_name_of_val(&e));
                        return Err(show_get_error(e).into());
                    }
                    anyhow::bail!("unknown error getting sizes");
                }
            };
            let _total_size = sizes.iter().copied().sum::<u64>();
            // For payload size, we want the actual file data size
            // The sizes array contains: [collection_size, file1_size, file2_size, ...]
            // We skip the first element (collection metadata) but include all file sizes
            let payload_size = sizes.iter().skip(1).copied().sum::<u64>();
            let total_files = sizes.len().saturating_sub(1) as u64;

            // Emit initial progress event (0%) so frontend can display total size immediately
            emit_progress_event(&app_handle, "receive-progress", 0, payload_size, 0.0);

            let _local_size = local.local_bytes();
            let get = db.remote().execute_get(connection, local.missing());
            let mut stats = Stats::default();
            let mut stream = get.stream();
            let mut last_log_offset = 0u64;
            let transfer_start_time = Instant::now();

            while let Some(item) = stream.next().await {
                trace!("got item {item:?}");
                match item {
                    GetProgressItem::Progress(offset) => {
                        // Emit progress events every 1MB
                        if offset - last_log_offset > 1_000_000 {
                            last_log_offset = offset;

                            // Calculate speed and emit progress event
                            let elapsed = transfer_start_time.elapsed().as_secs_f64();
                            let speed_bps = if elapsed > 0.0 {
                                offset as f64 / elapsed
                            } else {
                                0.0
                            };

                            emit_progress_event(&app_handle, "receive-progress", offset, payload_size, speed_bps);
                        }
                    }
                    GetProgressItem::Done(value) => {
                        stats = value;

                        // Emit final progress event
                        let elapsed = transfer_start_time.elapsed().as_secs_f64();
                        let speed_bps = if elapsed > 0.0 {
                            payload_size as f64 / elapsed
                        } else {
                            0.0
                        };
                        emit_progress_event(&app_handle, "receive-progress", payload_size, payload_size, speed_bps);

                        break;
                    }
                    GetProgressItem::Error(cause) => {
                        tracing::error!("Download error: {:?}", cause);
                        anyhow::bail!(show_get_error(cause));
                    }
                }
            }
            (stats, total_files, payload_size)
        } else {
            let total_files = local.children().unwrap() - 1;
            let payload_bytes = 0; // todo local.sizes().skip(2).map(Option::unwrap).sum::<u64>();

            // Emit events for already complete data
            emit_event(&app_handle, "receive-started");
            emit_event(&app_handle, "receive-completed");

            (Stats::default(), total_files, payload_bytes)
        };

        let collection = Collection::load(hash_and_format.hash, db.as_ref()).await?;

        // Extract file names from collection and emit them BEFORE export
        // This allows the UI to show file names during the export phase
        let mut file_names: Vec<String> = Vec::new();
        for (name, _hash) in collection.iter() {
            file_names.push(name.to_string());
        }

        // Emit file names information
        if !file_names.is_empty() {
            let file_names_json =
                serde_json::to_string(&file_names).unwrap_or_else(|_| "[]".to_string());
            emit_event_with_payload(&app_handle, "receive-file-names", &file_names_json);
        }

        // Determine output directory
        let output_dir = options.output_dir.unwrap_or_else(|| {
            dirs::download_dir().unwrap_or_else(|| std::env::current_dir().unwrap())
        });

        export(&db, collection, &output_dir).await?;

        // Emit completion event AFTER everything is done
        emit_event(&app_handle, "receive-completed");

        anyhow::Ok((total_files, payload_size, stats, output_dir))
    };

    let (total_files, payload_size, _stats, output_dir) = select! {
        x = fut => match x {
            Ok(x) => x,
            Err(e) => {
                tracing::error!("Download operation failed: {}", e);
                // make sure we shutdown the db before exiting
                db2.shutdown().await?;
                anyhow::bail!("error: {e}");
            }
        },
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!("Operation cancelled by user");
            db2.shutdown().await?;
            anyhow::bail!("Operation cancelled");
        }
    };

    tokio::fs::remove_dir_all(&iroh_data_dir).await?;

    Ok(ReceiveResult {
        message: format!("Downloaded {} files, {} bytes", total_files, payload_size),
        file_path: output_dir,
    })
}

/// 将集合中的各个 blob 导出到 `output_dir`。
///
/// 该函数会为每个条目创建目标路径并通过 `db.export_with_opts` 执行导出流。
async fn export(db: &Store, collection: Collection, output_dir: &Path) -> anyhow::Result<()> {
    for (name, hash) in collection.iter() {
        let target = get_export_path(output_dir, name)?;
        if target.exists() {
            anyhow::bail!("target {} already exists", target.display());
        }
        let mut stream = db
            .export_with_opts(ExportOptions {
                hash: *hash,
                target,
                mode: ExportMode::Copy,
            })
            .stream()
            .await;

        while let Some(item) = stream.next().await {
            match item {
                ExportProgressItem::Size(_size) => {
                    // Skip progress updates for library version
                }
                ExportProgressItem::CopyProgress(_offset) => {
                    // Skip progress updates for library version
                }
                ExportProgressItem::Done => {
                    // Export completed
                }
                ExportProgressItem::Error(cause) => {
                    anyhow::bail!("error exporting {}: {}", name, cause);
                }
            }
        }
    }
    Ok(())
}

/// 将 `GetError` 打印到日志并原样返回，便于上层处理。
fn show_get_error(e: GetError) -> GetError {
    match &e {
        GetError::InitialNext { source, .. } => {
            tracing::error!("initial connection error: {source}");
        }
        GetError::ConnectedNext { source, .. } => {
            tracing::error!("connected error: {source}");
        }
        GetError::AtBlobHeaderNext { source, .. } => {
            tracing::error!("reading blob header error: {source}");
        }
        GetError::Decode { source, .. } => {
            tracing::error!("decoding error: {source}");
        }
        GetError::IrpcSend { source, .. } => {
            tracing::error!("error sending over irpc: {source}");
        }
        GetError::AtClosingNext { source, .. } => {
            tracing::error!("error at closing: {source}");
        }
        GetError::BadRequest { .. } => {
            tracing::error!("bad request");
        }
        GetError::LocalFailure { source, .. } => {
            tracing::error!("local failure {source:?}");
        }
    }
    e
}

/// 根据集合内的名称生成导出路径，同时验证每个路径组件的合法性。
fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let parts = name.split('/');
    let mut path = root.to_path_buf();
    for part in parts {
        validate_path_component(part)?;
        path.push(part);
    }
    Ok(path)
}

/// 验证单个路径组件是否合法（不应包含分隔符 `/`）。
fn validate_path_component(component: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !component.contains('/'),
        "path components must not contain the only correct path separator, /"
    );
    Ok(())
}
