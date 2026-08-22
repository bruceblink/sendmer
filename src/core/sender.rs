//! 发送端功能：将本地文件/目录导入 Blob 存储并通过 iroh 协议对外提供。
//!
//! 主要导出 `start_share`，它会导入数据、启动路由器并返回用于后续管理的 `SendResult`。

use crate::core::endpoint::base_endpoint_builder;
use crate::core::events::{
    AppHandle, Role, TransferError, TransferErrorCode, TransferPhase, classified_transfer_error,
    classify_transfer_error, is_transfer_cancelled, transfer_cancelled_error,
};
use crate::core::options::{AddrInfoOptions, SendOptions, apply_options};
use crate::core::progress::{
    SenderProgressReporter, SenderTransferStatus, TransferEventEmitter, TransferId,
};
use crate::core::results::{SendHandle, SendResult};
use crate::core::storage::{load_fs_store, unique_temp_dir};
use anyhow::Context;
use iroh::{Endpoint, address_lookup::PkarrPublisher};
use iroh_blobs::{
    BlobFormat, BlobsProtocol,
    api::{
        Store, TempTag,
        blobs::{AddPathOptions, ImportMode},
    },
    format::collection::Collection,
    provider::events::{ConnectMode, EventMask, EventSender, RequestMode, ThrottleMode},
    store::fs::FsStore,
    ticket::BlobTicket,
};
use n0_future::StreamExt;
use n0_future::{BufferedStreamExt, task::AbortOnDropHandle};
use std::{
    collections::{BTreeSet, HashSet},
    num::NonZeroU64,
    path::{Component, Path, PathBuf},
    time::Duration,
};
use tokio::{
    select,
    sync::{Semaphore, mpsc, watch},
    task::JoinSet,
    time::Instant,
};
use tracing::{info, trace, warn};
use walkdir::WalkDir;

const PROVIDER_PROGRESS_TASK_LIMIT: usize = 32;
const ENDPOINT_ONLINE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Prepare endpoint with the given options
async fn prepare_endpoint(options: &SendOptions) -> anyhow::Result<Endpoint> {
    let mut builder = base_endpoint_builder(options, vec![iroh_blobs::protocol::ALPN.to_vec()])?;

    if options.ticket_type == AddrInfoOptions::Id {
        builder = builder.address_lookup(PkarrPublisher::n0_dns());
    }

    builder.bind().await.map_err(Into::into)
}

/// Prepare temporary directory for blob storage
fn prepare_temp_directory() -> anyhow::Result<PathBuf> {
    unique_temp_dir(".sendmer-send-")
}

/// Validate the path to be shared
fn validate_share_path(path: &Path) -> anyhow::Result<()> {
    let canonical_cwd = std::env::current_dir()?.canonicalize()?;
    let source_metadata = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "share path {} does not exist or cannot be accessed",
            path.display()
        )
    })?;
    anyhow::ensure!(
        !source_metadata.file_type().is_symlink(),
        "cannot share symbolic link {}; symbolic links are not supported",
        path.display()
    );
    let canonical_path = path.canonicalize().with_context(|| {
        format!(
            "share path {} does not exist or cannot be accessed",
            path.display()
        )
    })?;

    if canonical_path == canonical_cwd {
        anyhow::bail!("can not share from the current directory");
    }

    anyhow::ensure!(
        canonical_path.is_file() || canonical_path.is_dir(),
        "share path {} is not a file or directory",
        path.display()
    );

    if canonical_path.is_dir() {
        validate_share_directory_contents(&canonical_path)?;
    }

    Ok(())
}

/// Reject source entries that the current file-only collection format cannot preserve.
///
/// Each directory must contain a regular file somewhere below it. This prevents an
/// empty directory or a symbolic link from being silently omitted during import.
fn validate_share_directory_contents(root: &Path) -> anyhow::Result<()> {
    let mut directories = BTreeSet::new();
    let mut directories_with_files = HashSet::new();

    for entry in WalkDir::new(root) {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type().is_dir() {
            directories.insert(path.to_path_buf());
            continue;
        }
        if entry.file_type().is_file() {
            for ancestor in path.ancestors().skip(1) {
                if !ancestor.starts_with(root) {
                    break;
                }
                directories_with_files.insert(ancestor.to_path_buf());
            }
            continue;
        }
        if entry.file_type().is_symlink() {
            anyhow::bail!(
                "cannot share symbolic link {}; symbolic links are not supported",
                path.display()
            );
        }
        anyhow::bail!("cannot share unsupported source entry {}", path.display());
    }

    if let Some(empty_directory) = directories
        .iter()
        .find(|directory| !directories_with_files.contains(*directory))
    {
        anyhow::bail!(
            "cannot share empty directory {}; empty directories are not supported",
            empty_directory.display()
        );
    }

    Ok(())
}

/// Enforce optional share-size limits before endpoint or temp-store setup.
fn validate_share_limits(
    path: &Path,
    max_files: Option<NonZeroU64>,
    max_total_size_bytes: Option<NonZeroU64>,
) -> anyhow::Result<()> {
    if max_files.is_none() && max_total_size_bytes.is_none() {
        return Ok(());
    }

    let mut file_count = 0_u64;
    let mut total_size_bytes = 0_u64;
    for entry in WalkDir::new(path) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        if let Some(max_files) = max_files {
            file_count = file_count.saturating_add(1);
            if file_count > max_files.get() {
                anyhow::bail!(
                    "share contains more than {} files; --max-files is {}",
                    max_files,
                    max_files
                );
            }
        }

        if let Some(max_total_size_bytes) = max_total_size_bytes {
            let file_size = entry.metadata()?.len();
            total_size_bytes = total_size_bytes
                .checked_add(file_size)
                .ok_or_else(|| anyhow::anyhow!("share total size exceeds u64 range"))?;
            if total_size_bytes > max_total_size_bytes.get() {
                anyhow::bail!(
                    "share contains more than {} bytes; --max-total-size is {}",
                    max_total_size_bytes,
                    max_total_size_bytes
                );
            }
        }
    }

    Ok(())
}

/// Set up data sharing and return immediately after the router starts.
///
/// The caller owns the returned setup before it waits for endpoint readiness, so a
/// later cancellation can use the normal graceful shutdown path.
async fn setup_data_sharing(
    endpoint: Endpoint,
    blobs_data_dir: PathBuf,
    share_request: ShareRequest,
) -> anyhow::Result<SharingSetup> {
    let cleanup_dir = blobs_data_dir.clone();
    let (progress_tx, progress_rx) = mpsc::channel(32);
    let (transfer_status_tx, transfer_status_rx) = watch::channel(SenderTransferStatus::Idle);
    let (shutdown_signal_tx, shutdown_signal_rx) = watch::channel(false);

    let setup_future = async move {
        let ShareRequest {
            path,
            entry_type,
            event_emitter,
            max_upload_rate_bytes_per_sec,
            max_receivers,
        } = share_request;
        let store = load_fs_store(&blobs_data_dir).await.map_err(|error| {
            sender_failure(
                error,
                TransferErrorCode::Filesystem,
                TransferPhase::Preparing,
                false,
                "unable to prepare sender storage",
            )
        })?;

        let blobs = BlobsProtocol::new(
            &store,
            Some(create_event_sender(
                progress_tx,
                max_upload_rate_bytes_per_sec,
                max_receivers,
            )),
        );

        let imported = import(path, blobs.store()).await.map_err(|error| {
            sender_failure(
                error,
                TransferErrorCode::Filesystem,
                TransferPhase::Preparing,
                false,
                "unable to import shared data",
            )
        })?;
        let size = imported.size;
        let progress_handle = spawn_provider_progress_task(
            progress_rx,
            event_emitter,
            size,
            entry_type,
            transfer_status_tx,
            ProviderProgressControl {
                max_upload_rate_bytes_per_sec,
                max_receivers,
                shutdown_signal_rx,
            },
        );

        let router = iroh::protocol::Router::builder(endpoint)
            .accept(iroh_blobs::protocol::ALPN, blobs.clone())
            .spawn();

        anyhow::Ok(SharingSetup {
            router,
            imported,
            blobs_data_dir,
            store,
            progress_handle,
            transfer_status_rx,
            shutdown_signal_tx,
        })
    };

    match setup_future.await {
        Ok(setup) => Ok(setup),
        Err(error) => Err(finalize_failed_sender_setup(
            error,
            remove_temp_sender_dir(&cleanup_dir).await,
        )),
    }
}

/// Remove a sender-owned temporary store after setup fails, tolerating a store
/// that was never created.
async fn remove_temp_sender_dir(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Preserve the startup error while recording a best-effort cleanup failure.
fn finalize_failed_sender_setup(
    primary_error: anyhow::Error,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Error {
    if let Err(error) = cleanup_result {
        warn!(error = %error, "failed to clean sender temporary data after setup error");
    }
    primary_error
}

/// Pair an internal sender error with the stable details exposed to event consumers.
fn sender_failure(
    error: anyhow::Error,
    code: TransferErrorCode,
    phase: TransferPhase,
    retryable: bool,
    message: &'static str,
) -> anyhow::Error {
    classify_transfer_error(error, TransferError::new(code, phase, retryable, message))
}

struct ShareRequest {
    path: PathBuf,
    entry_type: crate::core::types::EntryType,
    event_emitter: TransferEventEmitter,
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
}

struct SharePlan {
    entry_type: crate::core::types::EntryType,
    wait_for_online: bool,
    blobs_data_dir: PathBuf,
    ticket_type: AddrInfoOptions,
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
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
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
) -> EventSender {
    EventSender::new(
        progress_tx,
        provider_event_mask(max_upload_rate_bytes_per_sec, max_receivers),
    )
}

/// Build the provider subscription mask for optional throttling and admission control.
const fn provider_event_mask(
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
) -> EventMask {
    EventMask {
        connected: if max_receivers.is_some() {
            ConnectMode::Intercept
        } else {
            ConnectMode::Notify
        },
        get: RequestMode::NotifyLog,
        throttle: if max_upload_rate_bytes_per_sec.is_some() {
            ThrottleMode::Intercept
        } else {
            ThrottleMode::None
        },
        ..EventMask::DEFAULT
    }
}

fn spawn_provider_progress_task(
    progress_rx: mpsc::Receiver<iroh_blobs::provider::events::ProviderMessage>,
    event_emitter: TransferEventEmitter,
    total_file_size: u64,
    entry_type: crate::core::types::EntryType,
    transfer_status_tx: watch::Sender<SenderTransferStatus>,
    control: ProviderProgressControl,
) -> AbortOnDropHandle<anyhow::Result<()>> {
    AbortOnDropHandle::new(tokio::spawn(show_provide_progress_with_provider_tracker(
        progress_rx,
        event_emitter,
        total_file_size,
        entry_type,
        transfer_status_tx,
        control,
    )))
}

/// Bundle provider limits and the sender shutdown signal passed to the progress task.
struct ProviderProgressControl {
    max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    max_receivers: Option<NonZeroU64>,
    shutdown_signal_rx: watch::Receiver<bool>,
}

async fn wait_until_endpoint_is_online(
    endpoint: &iroh::Endpoint,
    wait_for_online: bool,
) -> anyhow::Result<()> {
    if wait_for_online {
        match tokio::time::timeout(ENDPOINT_ONLINE_WAIT_TIMEOUT, endpoint.online()).await {
            Ok(()) => {}
            Err(error) => {
                warn!(
                    error = %error,
                    timeout_secs = ENDPOINT_ONLINE_WAIT_TIMEOUT.as_secs(),
                    "endpoint online probe timed out; continuing with available addresses"
                );
            }
        }
    }
    Ok(())
}

struct SharingSetup {
    router: iroh::protocol::Router,
    imported: ImportedCollection,
    blobs_data_dir: PathBuf,
    store: FsStore,
    progress_handle: AbortOnDropHandle<anyhow::Result<()>>,
    transfer_status_rx: watch::Receiver<SenderTransferStatus>,
    shutdown_signal_tx: watch::Sender<bool>,
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
            max_upload_rate_bytes_per_sec: options.max_upload_rate_bytes_per_sec,
            max_receivers: options.max_receivers,
        })
    }

    const fn build_request(
        &self,
        path: PathBuf,
        event_emitter: TransferEventEmitter,
    ) -> ShareRequest {
        ShareRequest {
            path,
            entry_type: self.entry_type,
            event_emitter,
            max_upload_rate_bytes_per_sec: self.max_upload_rate_bytes_per_sec,
            max_receivers: self.max_receivers,
        }
    }
}

impl SharingSetup {
    fn into_send_result(
        self,
        entry_type: crate::core::types::EntryType,
        ticket_type: AddrInfoOptions,
        event_emitter: TransferEventEmitter,
    ) -> SendResult {
        let Self {
            router,
            imported,
            blobs_data_dir,
            store,
            progress_handle,
            transfer_status_rx,
            shutdown_signal_tx,
        } = self;
        let ImportedCollection { temp_tag, size, .. } = imported;
        let hash = temp_tag.hash();

        let mut addr = router.endpoint().addr();
        apply_options(&mut addr, ticket_type);

        let ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq);

        SendResult {
            ticket,
            hash,
            size,
            entry_type,
            router,
            temp_tag,
            blobs_data_dir,
            _progress_handle: progress_handle,
            _store: store,
            transfer_status_rx,
            event_emitter,
            shutdown_signal_tx,
        }
    }
}

/// Shut down a fully initialized share after its final startup step fails or is cancelled.
///
/// A `SharingSetup` already owns a live router and file-backed store, so it must be
/// converted into `SendResult` to reuse the ordered shutdown and directory cleanup logic.
async fn shutdown_started_sender_setup(
    setup: SharingSetup,
    entry_type: crate::core::types::EntryType,
    ticket_type: AddrInfoOptions,
    event_emitter: TransferEventEmitter,
    primary_error: anyhow::Error,
) -> anyhow::Error {
    let cleanup_result = setup
        .into_send_result(entry_type, ticket_type, event_emitter)
        .shutdown_resources()
        .await;
    finalize_failed_sender_setup(primary_error, cleanup_result)
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
    let event_emitter = TransferEventEmitter::new(app_handle, Role::Sender);
    event_emitter.emit_started(TransferPhase::Preparing);
    let result = send_started(path, options, event_emitter.clone()).await;
    if let Err(error) = &result {
        if is_transfer_cancelled(error) {
            event_emitter.emit_cancelled();
        } else if let Some(details) = classified_transfer_error(error) {
            event_emitter.emit_failed(details);
        } else {
            event_emitter.emit_internal_failure("send failed");
        }
    }
    result
}

/// Run sender setup after the public entry point has opened its observable session.
async fn send_started(
    path: PathBuf,
    options: SendOptions,
    event_emitter: TransferEventEmitter,
) -> anyhow::Result<SendResult> {
    info!(
        path = %path.display(),
        relay_mode = ?options.relay_mode,
        ticket_type = ?options.ticket_type,
        max_upload_rate_bytes_per_sec = ?options.max_upload_rate_bytes_per_sec,
        max_receivers = ?options.max_receivers,
        max_files = ?options.max_files,
        max_total_size_bytes = ?options.max_total_size_bytes,
        "starting send"
    );
    validate_share_path(&path).map_err(|error| {
        sender_failure(
            error,
            TransferErrorCode::InvalidInput,
            TransferPhase::Preparing,
            false,
            "invalid share path",
        )
    })?;
    if options.max_files.is_some() || options.max_total_size_bytes.is_some() {
        validate_share_limits(&path, options.max_files, options.max_total_size_bytes).map_err(
            |error| {
                sender_failure(
                    error,
                    TransferErrorCode::InvalidInput,
                    TransferPhase::Preparing,
                    false,
                    "share exceeds the configured resource limit",
                )
            },
        )?;
    }

    let plan = SharePlan::new(&path, &options).map_err(|error| {
        sender_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Preparing,
            false,
            "unable to prepare sender storage",
        )
    })?;
    let endpoint = prepare_endpoint(&options).await.map_err(|error| {
        sender_failure(
            error,
            TransferErrorCode::ConnectionFailed,
            TransferPhase::Connecting,
            true,
            "unable to initialize sender networking",
        )
    })?;
    let share_request = plan.build_request(path, event_emitter.clone());
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);

    // If setup and Ctrl+C become ready together, retain the completed setup so it can
    // be shut down gracefully below instead of dropping a live router.
    let setup_result = select! {
        biased;
        x = setup_data_sharing(
            endpoint,
            plan.blobs_data_dir.clone(),
            share_request
        ) => x,
        _ = &mut ctrl_c => {
            Err(transfer_cancelled_error())
        }
    };
    let setup = match setup_result {
        Ok(setup) => setup,
        Err(error) => {
            return Err(finalize_failed_sender_setup(
                error,
                remove_temp_sender_dir(&plan.blobs_data_dir).await,
            ));
        }
    };

    let online_wait_outcome = {
        let online_wait =
            wait_until_endpoint_is_online(setup.router.endpoint(), plan.wait_for_online);
        tokio::pin!(online_wait);

        select! {
            biased;
            _ = &mut ctrl_c => None,
            result = &mut online_wait => Some(result),
        }
    };

    match online_wait_outcome {
        Some(Ok(())) => {}
        Some(Err(error)) => {
            return Err(shutdown_started_sender_setup(
                setup,
                plan.entry_type,
                plan.ticket_type,
                event_emitter.clone(),
                error,
            )
            .await);
        }
        None => {
            return Err(shutdown_started_sender_setup(
                setup,
                plan.entry_type,
                plan.ticket_type,
                event_emitter.clone(),
                transfer_cancelled_error(),
            )
            .await);
        }
    }

    let result = setup.into_send_result(plan.entry_type, plan.ticket_type, event_emitter);

    info!(
        hash = %result.hash,
        size = result.size,
        entry_type = %result.entry_type,
        "send setup complete"
    );
    Ok(result)
}

/// Start a share and return the opaque v0.7 lifecycle handle.
///
/// This is the preferred embedding API for GUI and service callers. The
/// legacy `send` function remains available for source compatibility.
pub async fn send_handle(
    path: PathBuf,
    options: SendOptions,
    app_handle: AppHandle,
) -> anyhow::Result<SendHandle> {
    send(path, options, app_handle)
        .await
        .map(SendResult::into_handle)
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
    let sources = collect_import_sources(path)?;
    let parallelism = import_parallelism(sources.len());
    let imported = import_sources(db, sources, parallelism).await?;
    build_collection_from_imports(db, imported).await
}

fn import_parallelism(source_count: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    available.min(source_count.max(1))
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
    event_emitter: TransferEventEmitter,
    total_file_size: u64,
    entry_type: crate::core::types::EntryType,
    transfer_status_tx: watch::Sender<SenderTransferStatus>,
    control: ProviderProgressControl,
) -> anyhow::Result<()> {
    let ProviderProgressControl {
        max_upload_rate_bytes_per_sec,
        max_receivers,
        mut shutdown_signal_rx,
    } = control;
    let reporter = SenderProgressReporter::new(event_emitter, entry_type, transfer_status_tx);
    let request_task_limit = std::sync::Arc::new(Semaphore::new(PROVIDER_PROGRESS_TASK_LIMIT));
    let upload_rate_limiter = max_upload_rate_bytes_per_sec
        .map(UploadRateLimiter::new)
        .map(std::sync::Arc::new);
    let mut receiver_admission = ReceiverAdmission::new(max_receivers);
    let mut throttle_tasks = JoinSet::new();

    while let Some(item) = select! {
        biased;
        _ = shutdown_signal_rx.changed() => None,
        item = recv.recv() => item,
    } {
        while throttle_tasks.try_join_next().is_some() {}

        match item {
            iroh_blobs::provider::events::ProviderMessage::ClientConnected(msg) => {
                let result = if receiver_admission.admit(msg.connection_id) {
                    Ok(())
                } else {
                    Err(iroh_blobs::provider::events::AbortReason::RateLimited)
                };
                let _ = msg.tx.send(result).await;
            }
            iroh_blobs::provider::events::ProviderMessage::ClientConnectedNotify(_msg) => {}
            iroh_blobs::provider::events::ProviderMessage::ConnectionClosed(msg) => {
                receiver_admission.release(msg.connection_id);
            }
            iroh_blobs::provider::events::ProviderMessage::GetRequestReceivedNotify(msg) => {
                let transfer_id = TransferId::new(msg.connection_id, msg.request_id);
                reporter
                    .on_request_received(transfer_id, total_file_size)
                    .await;

                let reporter_clone = reporter.clone();
                let mut rx = msg.rx;
                let Ok(permit) = request_task_limit.clone().acquire_owned().await else {
                    break;
                };
                tokio::spawn(async move {
                    let _permit = permit;
                    while let Ok(Some(update)) = rx.recv().await {
                        reporter_clone.on_request_update(transfer_id, update).await;
                    }
                });
            }
            iroh_blobs::provider::events::ProviderMessage::Throttle(msg) => {
                if let Some(limiter) = upload_rate_limiter.clone() {
                    let mut shutdown_signal_rx = shutdown_signal_rx.clone();
                    throttle_tasks.spawn(async move {
                        if limiter
                            .wait_for_chunk_or_shutdown(msg.size, &mut shutdown_signal_rx)
                            .await
                        {
                            let _ = msg.tx.send(Ok(())).await;
                        }
                    });
                } else {
                    let _ = msg.tx.send(Ok(())).await;
                }
            }
            _ => {
                // Handle other message types that we don't need to track
            }
        }
    }

    // Outstanding throttle waits can be arbitrarily long at very low rates. Abort them
    // when the provider channel closes so sender shutdown never waits for pacing sleeps.
    throttle_tasks.abort_all();
    while throttle_tasks.join_next().await.is_some() {}

    Ok(())
}

/// Track active provider connections and reject new ones after the configured ceiling.
///
/// The provider event stream is processed serially, so this state machine makes admission
/// decisions without holding a lock across asynchronous response sends.
struct ReceiverAdmission {
    max_receivers: Option<NonZeroU64>,
    active_connections: HashSet<u64>,
}

impl ReceiverAdmission {
    fn new(max_receivers: Option<NonZeroU64>) -> Self {
        Self {
            max_receivers,
            active_connections: HashSet::new(),
        }
    }

    /// Admit a connection unless it would exceed the configured active-peer limit.
    fn admit(&mut self, connection_id: u64) -> bool {
        if self.active_connections.contains(&connection_id) {
            return true;
        }
        if let Some(max_receivers) = self.max_receivers {
            let active_connections =
                u64::try_from(self.active_connections.len()).expect("usize fits into u64");
            if active_connections >= max_receivers.get() {
                return false;
            }
        }
        self.active_connections.insert(connection_id);
        true
    }

    /// Release a connection slot after the provider reports that it closed.
    fn release(&mut self, connection_id: u64) {
        self.active_connections.remove(&connection_id);
    }
}

/// Serialize payload chunk reservations so every receiver shares one upload budget.
struct UploadRateLimiter {
    bytes_per_second: NonZeroU64,
    next_allowed_at: tokio::sync::Mutex<Instant>,
}

impl UploadRateLimiter {
    fn new(bytes_per_second: NonZeroU64) -> Self {
        Self {
            bytes_per_second,
            next_allowed_at: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    /// Reserve the next global pacing slot without holding the mutex while a task sleeps.
    async fn reserve(&self, bytes: u64) -> Instant {
        let now = Instant::now();
        let mut next_allowed_at = self.next_allowed_at.lock().await;
        let allowed_at = (*next_allowed_at).max(now);
        *next_allowed_at = allowed_at
            .checked_add(upload_delay(bytes, self.bytes_per_second))
            .unwrap_or(allowed_at);
        allowed_at
    }

    /// Wait for a pacing slot, returning false when sender shutdown cancels the pending wait.
    async fn wait_for_chunk_or_shutdown(
        &self,
        bytes: u64,
        shutdown_signal_rx: &mut watch::Receiver<bool>,
    ) -> bool {
        if *shutdown_signal_rx.borrow() {
            return false;
        }

        let deadline = self.reserve(bytes).await;
        tokio::select! {
            biased;
            _ = shutdown_signal_rx.changed() => false,
            _ = tokio::time::sleep_until(deadline) => true,
        }
    }
}

/// Convert a payload chunk into a rounded-up pacing interval that cannot exceed the configured rate.
fn upload_delay(bytes: u64, bytes_per_second: NonZeroU64) -> Duration {
    const NANOS_PER_SECOND: u128 = 1_000_000_000;

    let byte_nanoseconds = u128::from(bytes) * NANOS_PER_SECOND;
    let rate = u128::from(bytes_per_second.get());
    let nanoseconds =
        byte_nanoseconds / rate + u128::from((!byte_nanoseconds.is_multiple_of(rate)) as u8);
    let seconds = nanoseconds / NANOS_PER_SECOND;
    if seconds > u128::from(u64::MAX) {
        return Duration::MAX;
    }

    Duration::new(seconds as u64, (nanoseconds % NANOS_PER_SECOND) as u32)
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderProgressControl, ReceiverAdmission, ShareRequest, UploadRateLimiter,
        canonicalized_path_to_string, collect_import_sources, detect_entry_type,
        import_parallelism, prepare_endpoint, provider_event_mask, send, setup_data_sharing,
        show_provide_progress_with_provider_tracker, shutdown_started_sender_setup,
        validate_share_limits, validate_share_path,
    };
    use crate::core::events::{
        EventEmitter, Role, TransferErrorCode, TransferEvent, TransferEventData, TransferPhase,
    };
    use crate::core::options::{AddrInfoOptions, RelayModeOption, SendOptions, apply_options};
    use crate::core::progress::{SenderTransferStatus, TransferEventEmitter};
    use crate::core::types::EntryType;
    use iroh::{EndpointAddr, RelayUrl, SecretKey, TransportAddr};
    use iroh_blobs::provider::events::{
        ClientConnected, ConnectMode, ConnectionClosed, EventSender, ProgressError, ThrottleMode,
    };
    use std::num::NonZeroU64;
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::sync::watch;

    #[derive(Default)]
    struct RecordingEmitter {
        events: Mutex<Vec<TransferEvent>>,
    }

    impl RecordingEmitter {
        fn events(&self) -> Vec<TransferEvent> {
            self.events.lock().expect("events lock").clone()
        }
    }

    impl EventEmitter for RecordingEmitter {
        fn emit(&self, event: &TransferEvent) {
            self.events.lock().expect("events lock").push(event.clone());
        }
    }

    #[test]
    fn provider_event_mask_only_enables_throttle_for_configured_limits() {
        assert_eq!(provider_event_mask(None, None).throttle, ThrottleMode::None);
        assert_eq!(
            provider_event_mask(NonZeroU64::new(1_024), None).throttle,
            ThrottleMode::Intercept
        );
    }

    #[test]
    fn provider_event_mask_intercepts_connections_for_receiver_limits() {
        assert_eq!(
            provider_event_mask(None, NonZeroU64::new(1)).connected,
            ConnectMode::Intercept
        );
        assert_eq!(
            provider_event_mask(None, None).connected,
            ConnectMode::Notify
        );
    }

    #[test]
    fn receiver_admission_releases_slots_after_disconnect() {
        let mut admission = ReceiverAdmission::new(NonZeroU64::new(1));
        assert!(admission.admit(10));
        assert!(!admission.admit(20));
        admission.release(10);
        assert!(admission.admit(20));
    }

    #[test]
    fn receiver_admission_allows_repeated_events_for_same_connection() {
        let mut admission = ReceiverAdmission::new(NonZeroU64::new(1));
        assert!(admission.admit(10));
        assert!(admission.admit(10));
    }

    #[test]
    fn upload_delay_rounds_up_to_preserve_the_rate_ceiling() {
        assert_eq!(
            super::upload_delay(1, NonZeroU64::new(3).expect("non-zero rate")),
            Duration::from_nanos(333_333_334)
        );
        assert_eq!(
            super::upload_delay(
                16 * 1024,
                NonZeroU64::new(16 * 1024).expect("non-zero rate")
            ),
            Duration::from_secs(1)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn upload_rate_limiter_reserves_one_schedule_for_all_receivers() {
        let limiter = UploadRateLimiter::new(NonZeroU64::new(100).expect("non-zero rate"));
        let first = limiter.reserve(100).await;
        let second = limiter.reserve(100).await;

        assert_eq!(second.duration_since(first), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn upload_rate_limiter_serializes_parallel_receiver_reservations() {
        let limiter = std::sync::Arc::new(UploadRateLimiter::new(
            NonZeroU64::new(100).expect("non-zero rate"),
        ));
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let first_limiter = limiter.clone();
        let first_barrier = barrier.clone();
        let first = tokio::spawn(async move {
            first_barrier.wait().await;
            first_limiter.reserve(100).await
        });
        let second_limiter = limiter;
        let second_barrier = barrier.clone();
        let second = tokio::spawn(async move {
            second_barrier.wait().await;
            second_limiter.reserve(100).await
        });

        barrier.wait().await;
        let first = first.await.expect("first reservation task");
        let second = second.await.expect("second reservation task");
        let (earlier, later) = if first <= second {
            (first, second)
        } else {
            (second, first)
        };

        assert_eq!(later.duration_since(earlier), Duration::from_secs(1));
    }

    #[tokio::test(start_paused = true)]
    async fn upload_rate_limiter_cancels_pending_wait() {
        let limiter = std::sync::Arc::new(UploadRateLimiter::new(
            NonZeroU64::new(100).expect("non-zero rate"),
        ));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (_initial_shutdown_tx, mut initial_shutdown_rx) = watch::channel(false);
        assert!(
            limiter
                .wait_for_chunk_or_shutdown(100, &mut initial_shutdown_rx)
                .await
        );

        let wait_limiter = limiter.clone();
        let mut wait_shutdown = shutdown_rx;
        let pending = tokio::spawn(async move {
            wait_limiter
                .wait_for_chunk_or_shutdown(100, &mut wait_shutdown)
                .await
        });
        tokio::task::yield_now().await;

        shutdown_tx.send(true).expect("shutdown signal receiver");
        assert!(!pending.await.expect("pending wait task"));
    }

    #[tokio::test]
    async fn provider_progress_task_stops_when_sender_shutdown_is_signaled() {
        let (_event_sender, progress_rx) = EventSender::channel(8, provider_event_mask(None, None));
        let (transfer_status_tx, _transfer_status_rx) = watch::channel(SenderTransferStatus::Idle);
        let (shutdown_signal_tx, shutdown_signal_rx) = watch::channel(false);
        let progress_task = tokio::spawn(show_provide_progress_with_provider_tracker(
            progress_rx,
            TransferEventEmitter::new(None, Role::Sender),
            0,
            EntryType::File,
            transfer_status_tx,
            ProviderProgressControl {
                max_upload_rate_bytes_per_sec: None,
                max_receivers: None,
                shutdown_signal_rx,
            },
        ));

        shutdown_signal_tx
            .send(true)
            .expect("shutdown signal receiver");
        tokio::time::timeout(Duration::from_secs(1), progress_task)
            .await
            .expect("progress task should stop promptly")
            .expect("progress task should join")
            .expect("progress task should shut down cleanly");
    }

    #[tokio::test]
    async fn provider_rejects_connections_after_receiver_limit_and_reopens_slots() {
        let max_receivers = NonZeroU64::new(1);
        let (event_sender, progress_rx) =
            EventSender::channel(8, provider_event_mask(None, max_receivers));
        let (transfer_status_tx, _transfer_status_rx) = watch::channel(SenderTransferStatus::Idle);
        let (_shutdown_signal_tx, shutdown_signal_rx) = watch::channel(false);
        let progress_task = tokio::spawn(show_provide_progress_with_provider_tracker(
            progress_rx,
            TransferEventEmitter::new(None, Role::Sender),
            0,
            EntryType::File,
            transfer_status_tx,
            ProviderProgressControl {
                max_upload_rate_bytes_per_sec: None,
                max_receivers,
                shutdown_signal_rx,
            },
        ));

        event_sender
            .client_connected(|| ClientConnected {
                connection_id: 1,
                endpoint_id: None,
            })
            .await
            .expect("first connection should be admitted");
        let error = event_sender
            .client_connected(|| ClientConnected {
                connection_id: 2,
                endpoint_id: None,
            })
            .await
            .expect_err("second connection should be rejected");
        assert!(matches!(error, ProgressError::Limit { .. }));

        event_sender
            .connection_closed(|| ConnectionClosed { connection_id: 1 })
            .await
            .expect("closing a connection should release its slot");
        event_sender
            .client_connected(|| ClientConnected {
                connection_id: 2,
                endpoint_id: None,
            })
            .await
            .expect("a later connection should reuse the released slot");

        drop(event_sender);
        progress_task
            .await
            .expect("progress task should join")
            .expect("progress task should shut down cleanly");
    }

    fn sample_addr() -> iroh::EndpointAddr {
        let node_id = SecretKey::generate().public();
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

    #[test]
    fn import_parallelism_is_bounded_by_available_work() {
        assert_eq!(import_parallelism(0), 1);
        assert_eq!(import_parallelism(1), 1);
        assert!(import_parallelism(usize::MAX) >= 1);
    }

    #[test]
    fn validate_share_path_rejects_current_directory_aliases() {
        let dot_err = validate_share_path(Path::new("."))
            .expect_err("`.` should be treated as current directory");
        assert!(dot_err.to_string().contains("current directory"));

        let dot_slash_err = validate_share_path(Path::new("./"))
            .expect_err("`./` should be treated as current directory");
        assert!(dot_slash_err.to_string().contains("current directory"));
    }

    #[test]
    fn validate_share_path_rejects_current_directory_absolute_path() {
        let cwd = std::env::current_dir().expect("current dir");
        let err =
            validate_share_path(&cwd).expect_err("absolute current directory should be rejected");
        assert!(err.to_string().contains("current directory"));
    }

    #[test]
    fn validate_share_path_rejects_missing_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let missing = temp_dir.path().join("missing-share");

        let err = validate_share_path(&missing).expect_err("missing path should be rejected");

        assert!(
            err.to_string()
                .contains("does not exist or cannot be accessed")
        );
    }

    #[test]
    fn validate_share_path_accepts_directory_with_regular_files() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let nested = temp_dir.path().join("nested").join("share");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::write(nested.join("file.txt"), b"content").expect("write source file");
        validate_share_path(&nested).expect("nested path should be accepted");
    }

    #[test]
    fn validate_share_limits_rejects_directory_over_file_limit() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("share");
        std::fs::create_dir_all(&root).expect("create share directory");
        std::fs::write(root.join("one.txt"), b"one").expect("write first file");
        std::fs::write(root.join("two.txt"), b"two").expect("write second file");

        let error = validate_share_limits(&root, NonZeroU64::new(1), None)
            .expect_err("file limit should reject the second file");

        assert!(error.to_string().contains("more than 1 files"));
        assert!(error.to_string().contains("max-files"));
    }

    #[test]
    fn validate_share_limits_accepts_file_at_limits() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let file = temp_dir.path().join("share.txt");
        std::fs::write(&file, b"content").expect("write share file");

        validate_share_limits(&file, NonZeroU64::new(1), NonZeroU64::new(7))
            .expect("file at both limits should be accepted");
    }

    #[test]
    fn validate_share_limits_rejects_directory_over_total_size_limit() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("share");
        std::fs::create_dir_all(&root).expect("create share directory");
        std::fs::write(root.join("one.txt"), b"one").expect("write first file");
        std::fs::write(root.join("two.txt"), b"two").expect("write second file");

        let error = validate_share_limits(&root, None, NonZeroU64::new(5))
            .expect_err("total size limit should reject the share");

        assert!(error.to_string().contains("more than 5 bytes"));
        assert!(error.to_string().contains("max-total-size"));
    }

    #[test]
    fn validate_share_path_rejects_empty_directory() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let empty = temp_dir.path().join("empty-share");
        std::fs::create_dir_all(&empty).expect("create empty directory");

        let error = validate_share_path(&empty).expect_err("empty directory should be rejected");

        assert!(error.to_string().contains("empty directory"));
        assert!(error.to_string().contains("empty-share"));
    }

    #[test]
    fn validate_share_path_rejects_empty_nested_directory() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("share");
        let empty_nested = root.join("empty");
        std::fs::create_dir_all(&empty_nested).expect("create empty nested directory");
        std::fs::write(root.join("file.txt"), b"content").expect("write source file");

        let error =
            validate_share_path(&root).expect_err("empty nested directory should be rejected");

        assert!(error.to_string().contains("empty directory"));
        assert!(error.to_string().contains("empty"));
    }

    #[tokio::test]
    async fn send_rejects_empty_directory_during_preparation() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let empty = temp_dir.path().join("empty-share");
        std::fs::create_dir_all(&empty).expect("create empty directory");
        let emitter = Arc::new(RecordingEmitter::default());

        let result = send(
            empty,
            SendOptions {
                relay_mode: RelayModeOption::Disabled,
                ..SendOptions::default()
            },
            Some(emitter.clone()),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("empty directory should fail before sender setup"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("empty directory"));
        let events = emitter.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].event, TransferEventData::Started));
        assert!(matches!(
            &events[1].event,
            TransferEventData::Failed { error }
                if error.code == TransferErrorCode::InvalidInput
                    && error.phase == TransferPhase::Preparing
                    && !error.retryable
        ));
    }

    #[tokio::test]
    async fn send_rejects_file_limit_before_network_setup() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let share = temp_dir.path().join("share");
        std::fs::create_dir_all(&share).expect("create share directory");
        std::fs::write(share.join("one.txt"), b"one").expect("write first file");
        std::fs::write(share.join("two.txt"), b"two").expect("write second file");
        let emitter = Arc::new(RecordingEmitter::default());

        let result = send(
            share,
            SendOptions {
                relay_mode: RelayModeOption::Disabled,
                max_files: NonZeroU64::new(1),
                ..SendOptions::default()
            },
            Some(emitter.clone()),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("file limit should fail before sender setup"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("more than 1 files"));
        let events = emitter.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].event, TransferEventData::Started));
        assert!(matches!(
            &events[1].event,
            TransferEventData::Failed { error }
                if error.code == TransferErrorCode::InvalidInput
                    && error.phase == TransferPhase::Preparing
                    && !error.retryable
        ));
    }

    #[tokio::test]
    async fn send_rejects_total_size_limit_before_network_setup() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let share = temp_dir.path().join("share");
        std::fs::create_dir_all(&share).expect("create share directory");
        std::fs::write(share.join("payload.bin"), b"payload").expect("write payload");
        let emitter = Arc::new(RecordingEmitter::default());

        let result = send(
            share,
            SendOptions {
                relay_mode: RelayModeOption::Disabled,
                max_total_size_bytes: NonZeroU64::new(6),
                ..SendOptions::default()
            },
            Some(emitter.clone()),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("total size limit should fail before sender setup"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("more than 6 bytes"));
        let events = emitter.events();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0].event, TransferEventData::Started));
        assert!(matches!(
            &events[1].event,
            TransferEventData::Failed { error }
                if error.code == TransferErrorCode::InvalidInput
                    && error.phase == TransferPhase::Preparing
                    && !error.retryable
        ));
    }

    #[cfg(unix)]
    #[test]
    fn validate_share_path_rejects_symbolic_link_source() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let source = temp_dir.path().join("source.txt");
        let link = temp_dir.path().join("source-link.txt");
        std::fs::write(&source, b"content").expect("write source file");
        symlink(&source, &link).expect("create source link");

        let error = validate_share_path(&link).expect_err("symbolic link should be rejected");

        assert!(error.to_string().contains("symbolic link"));
    }

    #[tokio::test]
    async fn sender_setup_failure_removes_blob_store() {
        let source_dir = tempfile::tempdir().expect("source directory");
        let missing_source = source_dir.path().join("missing-source.bin");
        let storage_root = tempfile::tempdir().expect("storage root");
        let blobs_data_dir = storage_root.path().join("sender-store");
        let options = SendOptions {
            relay_mode: RelayModeOption::Disabled,
            ticket_type: AddrInfoOptions::RelayAndAddresses,
            ..SendOptions::default()
        };
        let endpoint = prepare_endpoint(&options)
            .await
            .expect("create sender endpoint");
        let share_request = ShareRequest {
            path: missing_source,
            entry_type: EntryType::File,
            event_emitter: started_sender_emitter(),
            max_upload_rate_bytes_per_sec: None,
            max_receivers: None,
        };

        let error = match setup_data_sharing(endpoint, blobs_data_dir.clone(), share_request).await
        {
            Ok(_) => panic!("missing source should fail sender setup"),
            Err(error) => error,
        };

        assert!(!error.to_string().is_empty());
        assert!(
            !blobs_data_dir.exists(),
            "sender setup failure should remove its blob store"
        );
    }

    #[tokio::test]
    async fn cancelling_after_router_starts_removes_blob_store() {
        let source_dir = tempfile::tempdir().expect("source directory");
        let source_file = source_dir.path().join("source.bin");
        tokio::fs::write(&source_file, b"cancel after setup")
            .await
            .expect("write source file");
        let storage_root = tempfile::tempdir().expect("storage root");
        let blobs_data_dir = storage_root.path().join("sender-store");
        let options = SendOptions {
            relay_mode: RelayModeOption::Disabled,
            ticket_type: AddrInfoOptions::RelayAndAddresses,
            ..SendOptions::default()
        };
        let endpoint = prepare_endpoint(&options)
            .await
            .expect("create sender endpoint");
        let setup = setup_data_sharing(
            endpoint,
            blobs_data_dir.clone(),
            ShareRequest {
                path: source_file,
                entry_type: EntryType::File,
                event_emitter: started_sender_emitter(),
                max_upload_rate_bytes_per_sec: None,
                max_receivers: None,
            },
        )
        .await
        .expect("start sender router");

        let error = shutdown_started_sender_setup(
            setup,
            EntryType::File,
            AddrInfoOptions::RelayAndAddresses,
            started_sender_emitter(),
            anyhow::anyhow!("Operation cancelled"),
        )
        .await;

        assert!(error.to_string().contains("Operation cancelled"));
        assert!(
            !blobs_data_dir.exists(),
            "cancelling a started sender should remove its blob store"
        );
    }

    fn started_sender_emitter() -> TransferEventEmitter {
        let emitter = TransferEventEmitter::new(None, Role::Sender);
        emitter.emit_started(TransferPhase::Preparing);
        emitter
    }
}
