//! 发送端功能：将本地文件/目录导入 Blob 存储并通过 iroh 协议对外提供。
//!
//! 主要导出 `start_share`，它会导入数据、启动路由器并返回用于后续管理的 `SendResult`。

use crate::core::args::get_or_create_secret;
use crate::core::events::AppHandle;
use crate::core::options::{AddrInfoOptions, SendOptions, apply_options};
use crate::core::progress::{SenderProgressReporter, TransferId};
use crate::core::results::SendResult;
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
    time::Duration,
};
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
    share_request: ShareRequest,
    wait_for_online: bool,
) -> anyhow::Result<SharingSetup> {
    let (progress_tx, progress_rx) = mpsc::channel(32);

    let setup_future = async move {
        tokio::fs::create_dir_all(&blobs_data_dir).await?;

        let store = FsStore::load(&blobs_data_dir).await?;

        let blobs = BlobsProtocol::new(&store, Some(create_event_sender(progress_tx)));

        let imported = import(share_request.path, blobs.store()).await?;
        let size = imported.size;
        let progress_handle = spawn_provider_progress_task(
            progress_rx,
            share_request.app_handle,
            size,
            share_request.entry_type,
        );

        let router = iroh::protocol::Router::builder(endpoint)
            .accept(iroh_blobs::protocol::ALPN, blobs.clone())
            .spawn();

        wait_until_endpoint_is_online(router.endpoint(), wait_for_online).await?;

        anyhow::Ok(SharingSetup {
            router,
            imported,
            blobs_data_dir,
            store,
            progress_handle,
        })
    };

    setup_future.await
}

struct ShareRequest {
    path: PathBuf,
    entry_type: crate::core::types::EntryType,
    app_handle: AppHandle,
}

struct SharePlan {
    entry_type: crate::core::types::EntryType,
    wait_for_online: bool,
    blobs_data_dir: PathBuf,
    ticket_type: AddrInfoOptions,
}

struct ImportedSource {
    name: String,
    path: PathBuf,
}

struct ImportedBlob {
    name: String,
    temp_tag: TempTag,
    size: u64,
}

fn create_event_sender(
    progress_tx: mpsc::Sender<iroh_blobs::provider::events::ProviderMessage>,
) -> EventSender {
    EventSender::new(
        progress_tx,
        EventMask {
            connected: ConnectMode::Notify,
            get: RequestMode::NotifyLog,
            ..EventMask::DEFAULT
        },
    )
}

fn spawn_provider_progress_task(
    progress_rx: mpsc::Receiver<iroh_blobs::provider::events::ProviderMessage>,
    app_handle: AppHandle,
    total_file_size: u64,
    entry_type: crate::core::types::EntryType,
) -> AbortOnDropHandle<anyhow::Result<()>> {
    AbortOnDropHandle::new(tokio::spawn(show_provide_progress_with_provider_tracker(
        progress_rx,
        app_handle,
        total_file_size,
        entry_type,
    )))
}

async fn wait_until_endpoint_is_online(
    endpoint: &iroh::Endpoint,
    wait_for_online: bool,
) -> anyhow::Result<()> {
    if wait_for_online {
        tokio::time::timeout(Duration::from_secs(30), async move {
            let _ = endpoint.online().await;
        })
        .await?;
    }
    Ok(())
}

/// Create the final send result with ticket
struct SendArtifacts {
    router: iroh::protocol::Router,
    temp_tag: TempTag,
    size: u64,
    entry_type: crate::core::types::EntryType,
    blobs_data_dir: PathBuf,
    store: FsStore,
    progress_handle: AbortOnDropHandle<anyhow::Result<()>>,
}

struct SharingSetup {
    router: iroh::protocol::Router,
    imported: ImportedCollection,
    blobs_data_dir: PathBuf,
    store: FsStore,
    progress_handle: AbortOnDropHandle<anyhow::Result<()>>,
}

struct ImportedCollection {
    temp_tag: TempTag,
    size: u64,
    _collection: Collection,
}

impl SharePlan {
    fn new(path: &Path, options: &SendOptions) -> anyhow::Result<Self> {
        Ok(Self {
            entry_type: detect_entry_type(path),
            wait_for_online: !matches!(
                options.relay_mode,
                crate::core::options::RelayModeOption::Disabled
            ),
            blobs_data_dir: prepare_temp_directory()?,
            ticket_type: options.ticket_type,
        })
    }

    fn build_request(&self, path: PathBuf, app_handle: AppHandle) -> ShareRequest {
        ShareRequest {
            path,
            entry_type: self.entry_type,
            app_handle,
        }
    }
}

fn create_send_result(
    artifacts: SendArtifacts,
    ticket_type: AddrInfoOptions,
) -> anyhow::Result<SendResult> {
    let SendArtifacts {
        router,
        temp_tag,
        size,
        entry_type,
        blobs_data_dir,
        store,
        progress_handle,
    } = artifacts;
    let hash = temp_tag.hash();

    let mut addr = router.endpoint().addr();
    apply_options(&mut addr, ticket_type);

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
    validate_share_path(&path)?;

    let plan = SharePlan::new(&path, &options)?;
    let endpoint = prepare_endpoint(&options).await?;
    let share_request = plan.build_request(path, app_handle);

    let setup = select! {
        x = setup_data_sharing(
            endpoint,
            plan.blobs_data_dir.clone(),
            share_request,
            plan.wait_for_online
        ) => x?,
        _ = tokio::signal::ctrl_c() => {
            anyhow::bail!("Operation cancelled");
        }
    };

    create_send_result(
        SendArtifacts {
            router: setup.router,
            temp_tag: setup.imported.temp_tag,
            size: setup.imported.size,
            entry_type: plan.entry_type,
            blobs_data_dir: setup.blobs_data_dir,
            store: setup.store,
            progress_handle: setup.progress_handle,
        },
        plan.ticket_type,
    )
}

fn detect_entry_type(path: &Path) -> crate::core::types::EntryType {
    if path.is_file() {
        crate::core::types::EntryType::File
    } else {
        crate::core::types::EntryType::Directory
    }
}

/// 将 `path`（文件或目录）导入到给定的 `Store`，并返回导入后的集合信息。
async fn import(path: PathBuf, db: &Store) -> anyhow::Result<ImportedCollection> {
    let parallelism = num_cpus::get();
    let sources = collect_import_sources(path)?;
    let imported = import_sources(db, sources, parallelism).await?;
    build_collection_from_imports(db, imported).await
}

fn collect_import_sources(path: PathBuf) -> anyhow::Result<Vec<ImportedSource>> {
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root = path.parent().context("context get parent")?;

    WalkDir::new(path.clone())
        .into_iter()
        .map(|entry| {
            let entry = entry?;
            if !entry.file_type().is_file() {
                return Ok(None);
            }

            let path = entry.into_path();
            let relative = path.strip_prefix(root)?;
            let name = canonicalized_path_to_string(relative, true)?;
            anyhow::Ok(Some(ImportedSource { name, path }))
        })
        .filter_map(Result::transpose)
        .collect::<anyhow::Result<Vec<_>>>()
}

async fn import_sources(
    db: &Store,
    sources: Vec<ImportedSource>,
    parallelism: usize,
) -> anyhow::Result<Vec<ImportedBlob>> {
    n0_future::stream::iter(sources)
        .map(|source| {
            let db = db.clone();
            async move { import_source(&db, source).await }
        })
        .buffered_unordered(parallelism)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()
}

async fn import_source(db: &Store, source: ImportedSource) -> anyhow::Result<ImportedBlob> {
    let import = db.add_path_with_opts(AddPathOptions {
        path: source.path,
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
        trace!("importing {} {item:?}", source.name);
        match item {
            iroh_blobs::api::blobs::AddProgressItem::Size(size) => {
                item_size = size;
            }
            iroh_blobs::api::blobs::AddProgressItem::CopyProgress(_) => {}
            iroh_blobs::api::blobs::AddProgressItem::CopyDone => {}
            iroh_blobs::api::blobs::AddProgressItem::OutboardProgress(_) => {}
            iroh_blobs::api::blobs::AddProgressItem::Error(cause) => {
                anyhow::bail!("error importing {}: {}", source.name, cause);
            }
            iroh_blobs::api::blobs::AddProgressItem::Done(tt) => {
                break tt;
            }
        }
    };

    Ok(ImportedBlob {
        name: source.name,
        temp_tag,
        size: item_size,
    })
}

async fn build_collection_from_imports(
    db: &Store,
    mut imported: Vec<ImportedBlob>,
) -> anyhow::Result<ImportedCollection> {
    imported.sort_by(|a, b| a.name.cmp(&b.name));
    let size = imported.iter().map(|item| item.size).sum::<u64>();
    let (collection, tags) = imported
        .into_iter()
        .map(|item| ((item.name, item.temp_tag.hash()), item.temp_tag))
        .unzip::<_, _, Collection, Vec<_>>();
    let temp_tag = collection.clone().store(db).await?;
    drop(tags);
    Ok(ImportedCollection {
        temp_tag,
        size,
        _collection: collection,
    })
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
    let reporter = SenderProgressReporter::new(app_handle, entry_type);

    while let Some(item) = recv.recv().await {
        match item {
            iroh_blobs::provider::events::ProviderMessage::ClientConnectedNotify(_msg) => {}
            iroh_blobs::provider::events::ProviderMessage::ConnectionClosed(_msg) => {}
            iroh_blobs::provider::events::ProviderMessage::GetRequestReceivedNotify(msg) => {
                let transfer_id = TransferId::new(msg.connection_id, msg.request_id);
                reporter
                    .on_request_received(transfer_id, total_file_size)
                    .await;

                let reporter_clone = reporter.clone();
                let mut rx = msg.rx;
                tokio::spawn(async move {
                    while let Ok(Some(update)) = rx.recv().await {
                        reporter_clone.on_request_update(transfer_id, update).await;
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

#[cfg(test)]
mod tests {
    use super::{canonicalized_path_to_string, collect_import_sources, detect_entry_type};
    use crate::core::options::{AddrInfoOptions, apply_options};
    use crate::core::types::EntryType;
    use iroh::{EndpointAddr, RelayUrl, SecretKey, TransportAddr};
    use std::path::Path;
    use std::str::FromStr;

    fn sample_addr() -> iroh::EndpointAddr {
        let node_id = SecretKey::generate(&mut rand::rng()).public();
        let relay = RelayUrl::from_str("https://relay.example").expect("valid relay url");
        let ip = "127.0.0.1:7777".parse().expect("valid socket addr");
        EndpointAddr::new(node_id)
            .with_relay_url(relay)
            .with_ip_addr(ip)
    }

    #[test]
    fn apply_options_matches_ticket_type_semantics() {
        let base = sample_addr();

        let mut id_only = base.clone();
        apply_options(&mut id_only, AddrInfoOptions::Id);
        assert!(id_only.addrs.is_empty());

        let mut relay_only = base.clone();
        apply_options(&mut relay_only, AddrInfoOptions::Relay);
        assert!(
            relay_only
                .addrs
                .iter()
                .all(|addr| matches!(addr, TransportAddr::Relay(_)))
        );
        assert!(!relay_only.addrs.is_empty());

        let mut ip_only = base.clone();
        apply_options(&mut ip_only, AddrInfoOptions::Addresses);
        assert!(
            ip_only
                .addrs
                .iter()
                .all(|addr| matches!(addr, TransportAddr::Ip(_)))
        );
        assert!(!ip_only.addrs.is_empty());

        let mut full = base.clone();
        apply_options(&mut full, AddrInfoOptions::RelayAndAddresses);
        assert_eq!(full.addrs.len(), base.addrs.len());
    }

    #[test]
    fn disabled_relay_skips_online_wait() {
        let wait_for_online = !matches!(
            crate::core::options::RelayModeOption::Disabled,
            crate::core::options::RelayModeOption::Disabled
        );
        assert!(!wait_for_online);
    }

    #[test]
    fn canonicalized_relative_path_uses_forward_slashes() {
        let path = Path::new("folder").join("nested").join("file.txt");
        let value = canonicalized_path_to_string(&path, true).expect("path should convert");
        assert_eq!(value, "folder/nested/file.txt");
    }

    #[test]
    fn canonicalized_absolute_path_keeps_leading_slash_when_allowed() {
        let value = canonicalized_path_to_string(Path::new("/folder/file.txt"), false)
            .expect("absolute path should convert");
        assert_eq!(value, "/folder/file.txt");
    }

    #[test]
    fn canonicalized_absolute_path_is_rejected_when_relative_required() {
        let err = canonicalized_path_to_string(Path::new("/folder/file.txt"), true)
            .expect_err("absolute path should be rejected");
        assert!(err.to_string().contains("invalid path component"));
    }

    #[test]
    fn detect_entry_type_distinguishes_file_and_directory() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let file_path = temp_dir.path().join("demo.txt");
        std::fs::write(&file_path, b"demo").expect("write file");

        assert_eq!(detect_entry_type(&file_path), EntryType::File);
        assert_eq!(detect_entry_type(temp_dir.path()), EntryType::Directory);
    }

    #[test]
    fn collect_import_sources_returns_relative_sorted_names_after_sorting() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("data");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).expect("create dirs");
        std::fs::write(root.join("alpha.txt"), b"a").expect("write alpha");
        std::fs::write(nested.join("beta.txt"), b"b").expect("write beta");

        let mut names = collect_import_sources(root)
            .expect("sources")
            .into_iter()
            .map(|source| source.name)
            .collect::<Vec<_>>();
        names.sort();

        assert_eq!(names, vec!["data/alpha.txt", "data/nested/beta.txt"]);
    }
}
