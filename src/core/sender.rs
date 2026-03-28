//! 发送端功能：将本地文件/目录导入 Blob 存储并通过 iroh 协议对外提供。
//!
//! 主要导出 `start_share`，它会导入数据、启动路由器并返回用于后续管理的 `SendResult`。

use crate::core::events::{AppHandle, Role, TransferEvent, emit_event};
use crate::core::options::{AddrInfoOptions, SendOptions};
use crate::core::results::SendResult;
use crate::core::args::get_or_create_secret;
use anyhow::Context;
use data_encoding::HEXLOWER;
use iroh::{Endpoint, RelayMode, discovery::pkarr::PkarrPublisher};
use iroh_blobs::{
    BlobFormat, BlobsProtocol,
    api::{
        Store, TempTag,
        blobs::{AddPathOptions, ImportMode},
    },
    format::collection::Collection,
    provider::events::{ConnectMode, EventMask, EventSender, RequestMode},
    store::fs::FsStore,
    ticket::BlobTicket,
};
use n0_future::StreamExt;
use n0_future::{BufferedStreamExt, task::AbortOnDropHandle};
use rand::Rng;
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio::{select, sync::mpsc};
use tracing::trace;
use walkdir::WalkDir;

// use helpers from core::progress

/// Prepare endpoint with the given options
async fn prepare_endpoint(options: &SendOptions) -> anyhow::Result<Endpoint> {
    let secret_key = get_or_create_secret()?;
    let relay_mode: RelayMode = options.relay_mode.clone().into();

    let mut builder = Endpoint::builder()
        .alpns(vec![iroh_blobs::protocol::ALPN.to_vec()])
        .secret_key(secret_key)
        .relay_mode(relay_mode);

    if options.ticket_type == AddrInfoOptions::Id {
        builder = builder.discovery(PkarrPublisher::n0_dns());
    }
    if let Some(addr) = options.magic_ipv4_addr {
        builder = builder.bind_addr_v4(addr);
    }
    if let Some(addr) = options.magic_ipv6_addr {
        builder = builder.bind_addr_v6(addr);
    }

    builder.bind().await.map_err(Into::into)
}

/// Prepare temporary directory for blob storage
fn prepare_temp_directory() -> anyhow::Result<PathBuf> {
    let suffix = rand::rng().random::<[u8; 16]>();
    let temp_base = std::env::temp_dir();
    let blobs_data_dir = temp_base.join(format!(".sendmer-send-{}", HEXLOWER.encode(&suffix)));

    if blobs_data_dir.exists() {
        anyhow::bail!(
            "can not share twice from the same directory: {}",
            temp_base.display(),
        );
    }

    Ok(blobs_data_dir)
}

/// Validate the path to be shared
fn validate_share_path(path: &Path) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    if cwd.join(path) == cwd {
        anyhow::bail!("can not share from the current directory");
    }
    Ok(())
}

/// Setup data sharing with progress tracking
async fn setup_data_sharing(
    endpoint: Endpoint,
    blobs_data_dir: PathBuf,
    path: PathBuf,
    entry_type: crate::core::types::EntryType,
    app_handle: AppHandle,
) -> anyhow::Result<(iroh::protocol::Router, (TempTag, u64, Collection), PathBuf, FsStore, n0_future::task::AbortOnDropHandle<anyhow::Result<()>>)> {
    let (progress_tx, progress_rx) = mpsc::channel(32);

    let setup_future = async move {
        let t0 = Instant::now();
        tokio::fs::create_dir_all(&blobs_data_dir).await?;

        let store = FsStore::load(&blobs_data_dir).await?;

        let blobs = BlobsProtocol::new(
            &store,
            Some(EventSender::new(
                progress_tx,
                EventMask {
                    connected: ConnectMode::Notify,
                    get: RequestMode::NotifyLog,
                    ..EventMask::DEFAULT
                },
            )),
        );

        let import_result = import(path, blobs.store()).await?;
        let _dt = t0.elapsed();

        let (ref _temp_tag, size, ref _collection) = import_result;
        let progress_handle = AbortOnDropHandle::new(tokio::spawn(show_provide_progress_with_provider_tracker(
            progress_rx,
            app_handle,
            size,
            entry_type,
        )));

        let router = iroh::protocol::Router::builder(endpoint)
            .accept(iroh_blobs::protocol::ALPN, blobs.clone())
            .spawn();

        let ep = router.endpoint();
        tokio::time::timeout(Duration::from_secs(30), async move {
            let _ = ep.online().await;
        })
        .await?;

        anyhow::Ok((
            router,
            import_result,
            blobs_data_dir,
            store,
            progress_handle,
        ))
    };

    setup_future.await
}

/// Create the final send result with ticket
fn create_send_result(
    router: iroh::protocol::Router,
    temp_tag: TempTag,
    size: u64,
    entry_type: crate::core::types::EntryType,
    blobs_data_dir: PathBuf,
    store: FsStore,
    progress_handle: n0_future::task::AbortOnDropHandle<anyhow::Result<()>>,
) -> anyhow::Result<SendResult> {
    let hash = temp_tag.hash();

    let addr = router.endpoint().addr();

    let ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq);

    Ok(SendResult {
        ticket,
        hash,
        size,
        entry_type,
        router,
        temp_tag,
        blobs_data_dir,
        _progress_handle: progress_handle,
        _store: store,
    })
}

/// 开始共享（发送）指定的 `path`（文件或目录）。
///
/// - `path`：要分享的文件或目录路径。
/// - `options`：发送配置（转发模式、ticket 类型等）。
/// - `app_handle`：可选的事件发射器句柄，用于 UI/CLI 上报进度。
///
/// 返回 `SendResult`，其中包含票据、hash、大小以及需要保持存活的 router/store 句柄。
pub async fn send(
    path: PathBuf,
    options: SendOptions,
    app_handle: AppHandle,
) -> anyhow::Result<SendResult> {
    // Validate the path to be shared
    validate_share_path(&path)?;

    // Determine entry type
    let entry_type = if path.is_file() {
        crate::core::types::EntryType::File
    } else {
        crate::core::types::EntryType::Directory
    };

    // Prepare endpoint
    let endpoint = prepare_endpoint(&options).await?;

    // Prepare temporary directory
    let blobs_data_dir = prepare_temp_directory()?;

    // Setup data sharing with progress tracking
    let (router, (temp_tag, size, _collection), _blobs_data_dir, store, progress_handle) = select! {
        x = setup_data_sharing(endpoint, blobs_data_dir, path, entry_type, app_handle) => x?,
        _ = tokio::signal::ctrl_c() => {
            anyhow::bail!("Operation cancelled");
        }
    };

    // Create the final send result
    create_send_result(router, temp_tag, size, entry_type, _blobs_data_dir, store, progress_handle)
}

/// 将 `path`（文件或目录）导入到给定的 `Store`，并返回临时标签、总字节数和集合信息。
async fn import(path: PathBuf, db: &Store) -> anyhow::Result<(TempTag, u64, Collection)> {
    let parallelism = num_cpus::get();
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root = path.parent().context("context get parent")?;
    let files = WalkDir::new(path.clone()).into_iter();
    let data_sources: Vec<(String, PathBuf)> = files
        .map(|entry| {
            let entry = entry?;
            if !entry.file_type().is_file() {
                return Ok(None);
            }
            let path = entry.into_path();
            let relative = path.strip_prefix(root)?;
            let name = canonicalized_path_to_string(relative, true)?;
            anyhow::Ok(Some((name, path)))
        })
        .filter_map(Result::transpose)
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut names_and_tags = n0_future::stream::iter(data_sources)
        .map(|(name, path)| {
            let db = db.clone();
            async move {
                let import = db.add_path_with_opts(AddPathOptions {
                    path,
                    mode: ImportMode::TryReference,
                    format: BlobFormat::Raw,
                });
                let mut stream = import.stream().await;
                let mut item_size = 0;
                let temp_tag = loop {
                    let item = stream
                        .next()
                        .await
                        .context("import stream ended without a tag")?;
                    trace!("importing {name} {item:?}");
                    match item {
                        iroh_blobs::api::blobs::AddProgressItem::Size(size) => {
                            item_size = size;
                        }
                        iroh_blobs::api::blobs::AddProgressItem::CopyProgress(_) => {}
                        iroh_blobs::api::blobs::AddProgressItem::CopyDone => {}
                        iroh_blobs::api::blobs::AddProgressItem::OutboardProgress(_) => {}
                        iroh_blobs::api::blobs::AddProgressItem::Error(cause) => {
                            anyhow::bail!("error importing {}: {}", name, cause);
                        }
                        iroh_blobs::api::blobs::AddProgressItem::Done(tt) => {
                            break tt;
                        }
                    }
                };
                anyhow::Ok((name, temp_tag, item_size))
            }
        })
        .buffered_unordered(parallelism)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;

    names_and_tags.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));
    let size = names_and_tags.iter().map(|(_, _, size)| *size).sum::<u64>();
    let (collection, tags) = names_and_tags
        .into_iter()
        .map(|(name, tag, _)| ((name, tag.hash()), tag))
        .unzip::<_, _, Collection, Vec<_>>();
    let temp_tag = collection.clone().store(db).await?;
    drop(tags);
    Ok((temp_tag, size, collection))
}

/// 将已经标准化的路径转换为库内部使用的字符串表示，路径分隔使用 `/`。
///
/// - `must_be_relative`：如果为 true，则遇到根目录将返回错误（要求相对路径）。
pub fn canonicalized_path_to_string(
    path: impl AsRef<Path>,
    must_be_relative: bool,
) -> anyhow::Result<String> {
    let mut path_str = String::new();
    let parts = path
        .as_ref()
        .components()
        .filter_map(|c| match c {
            Component::Normal(x) => {
                let c = match x.to_str() {
                    Some(c) => c,
                    None => return Some(Err(anyhow::anyhow!("invalid character in path"))),
                };

                if !c.contains('/') && !c.contains('\\') {
                    Some(Ok(c))
                } else {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                }
            }
            Component::RootDir => {
                if must_be_relative {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                } else {
                    path_str.push('/');
                    None
                }
            }
            _ => Some(Err(anyhow::anyhow!("invalid path component {:?}", c))),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let parts = parts.join("/");
    path_str.push_str(&parts);
    Ok(path_str)
}

/// 从提供者事件流中读取进度信息并使用ProviderProgressTracker进行跟踪。
///
/// 该函数使用ProviderProgressTracker来管理多个并发传输的进度，并根据完成状态发射相应的事件。
async fn show_provide_progress_with_provider_tracker(
    mut recv: mpsc::Receiver<iroh_blobs::provider::events::ProviderMessage>,
    app_handle: AppHandle,
    total_file_size: u64,
    entry_type: crate::core::types::EntryType,
) -> anyhow::Result<()> {
    use crate::core::progress::{ProviderProgressTracker, TransferId, CompletionStatus};

    let tracker: Arc<Mutex<ProviderProgressTracker>> = Arc::new(Mutex::new(ProviderProgressTracker::new(entry_type)));
    let mut has_emitted_started = false;

    while let Some(item) = recv.recv().await {
        match item {
            iroh_blobs::provider::events::ProviderMessage::ClientConnectedNotify(_msg) => {}
            iroh_blobs::provider::events::ProviderMessage::ConnectionClosed(_msg) => {}
            iroh_blobs::provider::events::ProviderMessage::GetRequestReceivedNotify(msg) => {
                let transfer_id = TransferId::new(msg.connection_id, msg.request_id);
                {
                    let mut tracker = tracker.lock().await;
                    tracker.on_request_started(transfer_id, total_file_size);
                }

                if !has_emitted_started {
                    emit_event(&app_handle, &TransferEvent::Started { role: Role::Sender });
                    has_emitted_started = true;
                }

                let app_handle_clone = app_handle.clone();
                let tracker_clone = Arc::clone(&tracker);
                let mut rx = msg.rx;
                tokio::spawn(async move {
                    while let Ok(Some(update)) = rx.recv().await {
                        match update {
                            iroh_blobs::provider::events::RequestUpdate::Started(_) => {
                                // Transfer started - already handled above
                            }
                            iroh_blobs::provider::events::RequestUpdate::Progress(m) => {
                                let mut tracker = tracker_clone.lock().await;
                                if let Some((processed, total, speed)) = tracker.on_progress(transfer_id, m.end_offset) {
                                    emit_event(&app_handle_clone, &TransferEvent::Progress {
                                        role: Role::Sender,
                                        processed,
                                        total,
                                        speed,
                                    });
                                }
                            }
                            iroh_blobs::provider::events::RequestUpdate::Completed(_) => {
                                let mut tracker = tracker_clone.lock().await;
                                match tracker.on_request_completed(transfer_id) {
                                    CompletionStatus::Completed => {
                                        emit_event(&app_handle_clone, &TransferEvent::Completed { role: Role::Sender });
                                    }
                                    CompletionStatus::InProgress => {
                                        // Continue tracking
                                    }
                                    CompletionStatus::MoreRequestsArrivingSoon => {
                                        // Wait for more requests
                                    }
                                }
                            }
                            iroh_blobs::provider::events::RequestUpdate::Aborted(_) => {
                                let mut tracker = tracker_clone.lock().await;
                                tracker.on_request_aborted(transfer_id);
                                emit_event(&app_handle_clone, &TransferEvent::Failed {
                                    role: Role::Sender,
                                    message: "transfer aborted".to_string()
                                });
                            }
                        }
                    }
                });
            }
            _ => {
                // Handle other message types that we don't need to track
            }
        }
    }

    Ok(())
}
