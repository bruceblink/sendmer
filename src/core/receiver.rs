//! 接收端功能：根据票据连接远端并导出数据到本地目录。
//!
//! 主要导出 `download`，它负责建立连接、跟踪进度并将文件导出到目标目录。

use crate::core::endpoint::base_endpoint_builder;
use crate::core::events::{
    AppHandle, Role, TransferError, TransferErrorCode, TransferPhase, classified_transfer_error,
    classify_transfer_error, is_transfer_cancelled, transfer_cancelled_error,
};
use crate::core::options::{ReceiveOptions, ReceiveRetryPolicy};
use crate::core::progress::{ReceiverProgressReporter, TransferEventEmitter};
use crate::core::receive_cache::ReceiveCacheLease;
use crate::core::results::ReceiveResult;
use crate::core::storage::{load_fs_store, unique_temp_dir};
use anyhow::Context;
use iroh::{Endpoint, address_lookup::DnsAddressLookup};
use iroh_blobs::{
    api::{
        Store,
        blobs::{ExportMode, ExportOptions, ExportProgressItem},
        remote::GetProgressItem,
    },
    format::collection::Collection,
    get::{GetError, request::get_hash_seq_and_sizes},
    ticket::BlobTicket,
};
use n0_future::StreamExt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc as StdArc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::select;
use tracing::info;
use tracing::log::trace;

// event helpers provided by `core::progress`

const RECEIVE_TEMP_DIR_PREFIX: &str = ".sendmer-recv-";

/// 下载并导出由 `ticket_str` 指定的数据到本地目录。
///
/// - `ticket_str`：连接票据字符串。
/// - `options`：接收选项（输出目录、转发模式等）。
/// - `app_handle`：可选的事件发射器句柄，用于 UI/CLI 上报进度与文件名等信息。
pub async fn receive(
    ticket_str: String,
    options: ReceiveOptions,
    app_handle: AppHandle,
) -> anyhow::Result<ReceiveResult> {
    receive_with_cancellation(ticket_str, options, app_handle, None).await
}

/// Download a ticket while allowing the caller to request graceful cleanup.
///
/// The optional watch receiver is intentionally separate from `ReceiveOptions` so existing
/// callers keep compiling. Cancellation follows the same cleanup path as Ctrl+C: the receive
/// endpoint, store, and temporary directory are closed before the future returns.
pub async fn receive_with_cancellation(
    ticket_str: String,
    options: ReceiveOptions,
    app_handle: AppHandle,
    cancellation: Option<tokio::sync::watch::Receiver<bool>>,
) -> anyhow::Result<ReceiveResult> {
    let event_emitter = TransferEventEmitter::new(app_handle, Role::Receiver);
    event_emitter.emit_started(TransferPhase::Preparing);
    let ticket = match BlobTicket::from_str(&ticket_str).map_err(|error| {
        receive_failure(
            error.into(),
            TransferErrorCode::InvalidInput,
            TransferPhase::Preparing,
            false,
            "invalid receive ticket",
        )
    }) {
        Ok(ticket) => ticket,
        Err(error) => {
            emit_receive_failed(&event_emitter, &error);
            return Err(error);
        }
    };
    info!(
        hash = %ticket.hash(),
        relay_addrs = ticket.addr().relay_urls().count(),
        ip_addrs = ticket.addr().ip_addrs().count(),
        "starting receive"
    );
    let output_dir = match resolve_output_dir(options.output_dir.clone()).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Preparing,
            false,
            "unable to resolve receive output directory",
        )
    }) {
        Ok(output_dir) => output_dir,
        Err(error) => {
            emit_receive_failed(&event_emitter, &error);
            return Err(error);
        }
    };
    let mut context = match ReceiveContext::prepare(ticket, &options).await {
        Ok(context) => context,
        Err(error) => {
            emit_receive_failed(&event_emitter, &error);
            return Err(error);
        }
    };

    let receive_result = select! {
        result = receive_once(&context, &output_dir, event_emitter.clone()) => result,
        _ = wait_for_cancellation(cancellation) => {
            tracing::warn!("operation cancelled by caller");
            Err(transfer_cancelled_error())
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!("operation cancelled by user");
            Err(transfer_cancelled_error())
        }
    };
    let artifacts = match receive_result {
        Ok(artifacts) => artifacts,
        Err(error) => {
            tracing::error!(error = %receive_failed_message(&error), "download operation failed");
            let cancelled = is_transfer_cancelled(&error);
            let error = finalize_failed_receive(error, cleanup_failed_receive(&mut context).await);
            if cancelled {
                event_emitter.emit_cancelled();
                return Err(error);
            }
            let message = receive_failed_message(&error);
            let error = error.context(message);
            emit_receive_failed(&event_emitter, &error);
            return Err(error);
        }
    };

    let result = match finish_receive(&mut context, artifacts).await {
        Ok(result) => result,
        Err(error) => {
            let error = receive_failure(
                error,
                TransferErrorCode::Internal,
                TransferPhase::Finalizing,
                false,
                "unable to finalize received data",
            );
            let message = receive_failed_message(&error);
            let error = error.context(message);
            emit_receive_failed(&event_emitter, &error);
            return Err(error);
        }
    };
    event_emitter.emit_completed();
    info!(output = %result.file_path.display(), message = %result.message, "receive completed");
    Ok(result)
}

/// Waits until the optional cancellation watch channel is set to true.
///
/// A missing receiver becomes a pending future, so the normal `receive` API keeps its existing
/// Ctrl+C-only behavior while the GPUI adapter can opt into graceful cancellation.
async fn wait_for_cancellation(cancellation: Option<tokio::sync::watch::Receiver<bool>>) {
    let Some(mut cancellation) = cancellation else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        if *cancellation.borrow() {
            return;
        }
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

/// 将集合中的各个 blob 导出到 `output_dir`。
///
/// 该函数会为每个条目创建目标路径并通过 `db.export_with_opts` 执行导出流。
/// Export a collection into staging, then commit complete top-level roots.
///
/// Final paths are untouched until every blob reports `Done`; a failed export only
/// removes the staging directory and therefore cannot leave a partial user file.
async fn export_atomically(
    db: &Store,
    collection: Collection,
    output_dir: &Path,
) -> anyhow::Result<()> {
    let root = collection_root_name(&collection)?;
    ensure_export_root_available(output_dir, &root)?;
    let staging_dir = create_staging_dir(output_dir)?;

    if let Err(error) = export_to_staging(db, &collection, &staging_dir).await {
        cleanup_staging_dir(&staging_dir);
        return Err(error);
    }

    if let Err(error) = commit_staged_export(&staging_dir, output_dir, &root) {
        cleanup_staging_dir(&staging_dir);
        return Err(error);
    }

    Ok(())
}

/// Export every collection entry below the staging root without touching final paths.
async fn export_to_staging(
    db: &Store,
    collection: &Collection,
    staging_dir: &Path,
) -> anyhow::Result<()> {
    for (name, hash) in collection.iter() {
        let target = get_export_path(staging_dir, name).map_err(|error| {
            receive_failure(
                error,
                TransferErrorCode::RemoteRejected,
                TransferPhase::Exporting,
                false,
                "received collection contains invalid paths",
            )
        })?;
        let mut stream = db
            .export_with_opts(ExportOptions {
                hash: *hash,
                target,
                mode: ExportMode::Copy,
            })
            .stream()
            .await;
        process_export_stream(&mut stream, name).await?;
    }
    Ok(())
}

/// Return the only top-level root a collection may export atomically.
///
/// sendmer creates one-root collections for both files and directories.  External
/// multi-root collections are rejected instead of risking a partial commit.
fn collection_root_name(collection: &Collection) -> anyhow::Result<String> {
    let mut roots = std::collections::BTreeSet::new();
    for (name, _) in collection.iter() {
        let root = name
            .split('/')
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| anyhow::anyhow!("collection contains invalid entry name"))?;
        validate_path_component(root)?;
        roots.insert(root.to_owned());
    }

    let Some(root) = roots.pop_first() else {
        anyhow::bail!("collection is empty")
    };
    anyhow::ensure!(
        roots.is_empty(),
        "collection contains multiple top-level roots and cannot be exported atomically"
    );
    Ok(root)
}

fn ensure_export_root_available(output_dir: &Path, root: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(output_dir)?;
    anyhow::ensure!(
        output_dir.is_dir(),
        "output root {} is not a directory",
        output_dir.display()
    );

    validate_path_component(root)?;
    let target = output_dir.join(root);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => return Err(target_conflict_error(&target)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    Ok(())
}

/// Create a unique staging directory under the destination so final commit uses rename.
fn create_staging_dir(output_dir: &Path) -> anyhow::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..100u32 {
        let candidate = output_dir.join(format!(".sendmer-stage-{stamp}-{attempt}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }

    anyhow::bail!("could not create a unique export staging directory")
}

fn cleanup_staging_dir(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "failed to clean export staging directory"
        );
    }
}

/// Commit the complete staging root without replacing a target created concurrently.
fn commit_staged_export(staging_dir: &Path, output_dir: &Path, root: &str) -> anyhow::Result<()> {
    validate_path_component(root)?;
    let staged = staging_dir.join(root);
    anyhow::ensure!(staged.exists(), "staged export root {root} is missing");
    let target = output_dir.join(root);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => return Err(target_conflict_error(&target)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    if let Err(error) = move_staged_root_without_replacing(&staged, &target) {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(receive_failure(
                error.into(),
                TransferErrorCode::TargetConflict,
                TransferPhase::Exporting,
                false,
                "receive target already exists",
            ));
        }
        return Err(error.into());
    }

    // Moving the root is the commit point. Cleanup can fail after a successful move
    // (for example, because another process created a file in staging), but must not
    // turn a completed export into a retryable failure.
    cleanup_staging_dir(staging_dir);
    Ok(())
}

fn target_conflict_error(target: &Path) -> anyhow::Error {
    receive_failure(
        anyhow::anyhow!("target {} already exists", target.display()),
        TransferErrorCode::TargetConflict,
        TransferPhase::Exporting,
        false,
        "receive target already exists",
    )
}

/// Move a staged export into place while preserving any target created by another process.
///
/// All release targets have a native no-replace rename primitive.  The fallback
/// retains the preflight check for unsupported platforms, but is not used by releases.
#[cfg(target_os = "linux")]
fn move_staged_root_without_replacing(source: &Path, target: &Path) -> std::io::Result<()> {
    let source = path_to_c_string(source)?;
    let target = path_to_c_string(target)?;
    // SAFETY: the paths are valid NUL-terminated strings and AT_FDCWD makes them absolute.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn move_staged_root_without_replacing(source: &Path, target: &Path) -> std::io::Result<()> {
    let source = path_to_c_string(source)?;
    let target = path_to_c_string(target)?;
    // SAFETY: the paths are valid NUL-terminated strings for renamex_np.
    let result = unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn path_to_c_string(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "export path contains an interior NUL byte",
        )
    })
}

#[cfg(windows)]
fn move_staged_root_without_replacing(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both UTF-16 paths are NUL-terminated and no replacement flag is set.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn move_staged_root_without_replacing(source: &Path, target: &Path) -> std::io::Result<()> {
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("target {} already exists", target.display()),
        ));
    }
    std::fs::rename(source, target)
}

/// 消费一个文件的导出进度流，并且只在收到 `Done` 后报告成功。
///
/// 底层通道提前关闭时，可能已经留下不完整的目标文件；调用方会把该错误纳入
/// 接收失败清理流程，而不会把半导出误报为成功。
async fn process_export_stream<S>(stream: &mut S, name: &str) -> anyhow::Result<()>
where
    S: n0_future::Stream<Item = ExportProgressItem> + Unpin + Send,
{
    let mut seen_done = false;
    while let Some(item) = stream.next().await {
        match item {
            ExportProgressItem::Size(_) | ExportProgressItem::CopyProgress(_) => {
                // This library version does not expose export progress to the caller.
            }
            ExportProgressItem::Done => {
                seen_done = true;
                break;
            }
            ExportProgressItem::Error(cause) => {
                anyhow::bail!("error exporting {name}: {cause}");
            }
        }
    }

    anyhow::ensure!(seen_done, "export stream ended before completion");
    Ok(())
}

struct ReceiveContext {
    ticket: BlobTicket,
    addr: iroh::EndpointAddr,
    endpoint: Endpoint,
    iroh_data_dir: PathBuf,
    db: Store,
    retry_policy: ReceiveRetryPolicy,
    cache_lease: Option<ReceiveCacheLease>,
}

struct ReceiveArtifacts {
    total_files: u64,
    payload_size: u64,
    root_item_path: PathBuf,
}

struct DownloadOutcome {
    total_files: u64,
    payload_size: u64,
}

struct DownloadPlan {
    total_files: u64,
    payload_size: u64,
    transfer_total: u64,
}

impl ReceiveContext {
    async fn prepare(ticket: BlobTicket, options: &ReceiveOptions) -> anyhow::Result<Self> {
        options.retry_policy.validate().map_err(|error| {
            receive_failure(
                error,
                TransferErrorCode::InvalidInput,
                TransferPhase::Preparing,
                false,
                "invalid receive retry policy",
            )
        })?;
        if let Some(cache) = &options.receive_cache {
            cache.validate().map_err(|error| {
                receive_failure(
                    error,
                    TransferErrorCode::InvalidInput,
                    TransferPhase::Preparing,
                    false,
                    "invalid receive cache options",
                )
            })?;
        }
        let addr = ticket.addr().clone();
        let (endpoint, iroh_data_dir, db, cache_lease) = prepare_env(&ticket, options).await?;
        Ok(Self {
            ticket,
            addr,
            endpoint,
            iroh_data_dir,
            db,
            retry_policy: options.retry_policy,
            cache_lease,
        })
    }

    fn hash_and_format(&self) -> iroh_blobs::HashAndFormat {
        self.ticket.hash_and_format()
    }

    async fn load_collection(&self) -> anyhow::Result<Collection> {
        Collection::load(self.hash_and_format().hash, &self.db)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
            .map_err(|error| {
                receive_failure(
                    error,
                    TransferErrorCode::TransferInterrupted,
                    TransferPhase::Metadata,
                    false,
                    "received collection metadata is unavailable",
                )
            })
    }
}

impl DownloadPlan {
    /// Build transfer totals from a collection hash sequence and its child sizes.
    ///
    /// The first child is collection metadata, so it is excluded from the user-facing
    /// file count and payload size. The root hash sequence itself still travels over
    /// the network and is included in progress accounting.
    fn from_hash_seq_and_sizes(hash_seq: &iroh_blobs::hashseq::HashSeq, sizes: &[u64]) -> Self {
        Self {
            total_files: sizes.len().saturating_sub(1) as u64,
            payload_size: sizes.iter().skip(1).copied().sum::<u64>(),
            transfer_total: (hash_seq.len() as u64)
                .saturating_mul(32)
                .saturating_add(sizes.iter().copied().sum::<u64>()),
        }
    }
}

async fn receive_once(
    context: &ReceiveContext,
    output_dir: &Path,
    event_emitter: TransferEventEmitter,
) -> anyhow::Result<ReceiveArtifacts> {
    trace!("load done!");

    let download = download_missing_data(context, event_emitter.clone()).await?;
    let collection = context
        .load_collection()
        .await
        .context("load received collection")?;
    emit_collection_file_names(&event_emitter, &collection);
    let root_item_path = resolve_root_item_path(output_dir, &collection)
        .context("resolve received output path")
        .map_err(|error| {
            receive_failure(
                error,
                TransferErrorCode::RemoteRejected,
                TransferPhase::Metadata,
                false,
                "received collection contains unsupported paths",
            )
        })?;
    export_atomically(&context.db, collection, output_dir)
        .await
        .context("export received files")
        .map_err(|error| {
            receive_failure(
                error,
                TransferErrorCode::Filesystem,
                TransferPhase::Exporting,
                false,
                "unable to export received files",
            )
        })?;

    Ok(ReceiveArtifacts {
        total_files: download.total_files,
        payload_size: download.payload_size,
        root_item_path,
    })
}

fn emit_collection_file_names(emitter: &TransferEventEmitter, collection: &Collection) {
    let file_names = collect_file_names(collection);
    if !file_names.is_empty() {
        emitter.emit_file_names(file_names);
    }
}

/// Format a user-facing failure while keeping both stage context and root cause.
fn receive_failed_message(error: &anyhow::Error) -> String {
    let details = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ");
    format!("error: {details}")
}

fn emit_receive_failed(emitter: &TransferEventEmitter, error: &anyhow::Error) {
    if let Some(details) = classified_transfer_error(error) {
        emitter.emit_failed(details);
    } else {
        emitter.emit_internal_failure("receive failed");
    }
}

/// Pair an internal receive error with the stable details exposed to event consumers.
fn receive_failure(
    error: anyhow::Error,
    code: TransferErrorCode,
    phase: TransferPhase,
    retryable: bool,
    message: &'static str,
) -> anyhow::Error {
    classify_transfer_error(error, TransferError::new(code, phase, retryable, message))
}

/// Classify protocol failures without asking consumers to parse `GetError` text.
fn receive_protocol_failure(
    error: anyhow::Error,
    phase: TransferPhase,
    fallback_message: &'static str,
) -> anyhow::Error {
    let (code, retryable, message) = match error.downcast_ref::<GetError>() {
        Some(GetError::BadRequest { .. }) => (
            TransferErrorCode::RemoteRejected,
            false,
            "sender rejected the transfer request",
        ),
        Some(GetError::LocalFailure { .. }) => (
            TransferErrorCode::Internal,
            false,
            "local transfer processing failed",
        ),
        _ => (
            TransferErrorCode::TransferInterrupted,
            true,
            fallback_message,
        ),
    };
    receive_failure(error, code, phase, retryable, message)
}

fn finalize_failed_receive(
    primary_error: anyhow::Error,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Error {
    if let Err(error) = cleanup_result {
        tracing::warn!(error = %error, "failed to cleanup receive context after error");
    }
    primary_error
}

async fn cleanup_failed_receive(context: &mut ReceiveContext) -> anyhow::Result<()> {
    close_receive_endpoint(&context.endpoint).await;
    let shutdown_result = context.db.shutdown().await.map_err(anyhow::Error::from);
    let cleanup_result = finalize_receive_storage(context, false).await;
    finalize_cleanup(shutdown_result, cleanup_result)
}

async fn finish_receive(
    context: &mut ReceiveContext,
    artifacts: ReceiveArtifacts,
) -> anyhow::Result<ReceiveResult> {
    close_receive_endpoint(&context.endpoint).await;
    let shutdown_result = context.db.shutdown().await.map_err(anyhow::Error::from);
    let cleanup_result = finalize_receive_storage(context, shutdown_result.is_ok()).await;
    finalize_cleanup(shutdown_result, cleanup_result)?;

    Ok(ReceiveResult {
        message: format!(
            "Downloaded {} files, {} bytes",
            artifacts.total_files, artifacts.payload_size
        ),
        file_path: artifacts.root_item_path,
    })
}

/// Preserve persistent data after failure, but remove it after a clean export.
async fn finalize_receive_storage(
    context: &mut ReceiveContext,
    receive_succeeded: bool,
) -> anyhow::Result<()> {
    match context.cache_lease.take() {
        Some(lease) if receive_succeeded => lease.remove().await,
        Some(lease) => lease.preserve(),
        None => remove_temp_receive_dir(&context.iroh_data_dir).await,
    }
}

/// Close the receive endpoint before its temporary store is finalized.
async fn close_receive_endpoint(endpoint: &Endpoint) {
    if endpoint.is_closed() {
        return;
    }

    if tokio::time::timeout(std::time::Duration::from_secs(2), endpoint.close())
        .await
        .is_err()
    {
        tracing::warn!("timed out while closing receive endpoint");
    }
}

async fn remove_temp_receive_dir(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn download_missing_data(
    context: &ReceiveContext,
    emitter: TransferEventEmitter,
) -> anyhow::Result<DownloadOutcome> {
    let hash_and_format = context.hash_and_format();
    let local = context
        .db
        .remote()
        .local(hash_and_format)
        .await
        .map_err(|error| {
            receive_failure(
                error.into(),
                TransferErrorCode::Filesystem,
                TransferPhase::Transferring,
                false,
                "unable to inspect receiver storage",
            )
        })?;
    if local.is_complete() {
        let total_files = completed_local_total_files_from_children(local.children())?;
        return Ok(DownloadOutcome {
            total_files,
            payload_size: 0,
        });
    }

    let (hash_seq, sizes) = get_sizes_with_retries(
        &context.endpoint,
        &context.addr,
        &context.ticket.hash(),
        context.retry_policy,
    )
    .await
    .context("fetch remote collection sizes")
    .map_err(|error| {
        receive_protocol_failure(
            error,
            TransferPhase::Metadata,
            "collection metadata transfer was interrupted",
        )
    })?;
    let plan = DownloadPlan::from_hash_seq_and_sizes(&hash_seq, &sizes);
    execute_download_with_retries(context, &plan, &emitter).await?;

    Ok(DownloadOutcome {
        total_files: plan.total_files,
        payload_size: plan.payload_size,
    })
}

const fn completed_local_total_files(children: u64) -> u64 {
    children.saturating_sub(1)
}

fn completed_local_total_files_from_children(children: Option<u64>) -> anyhow::Result<u64> {
    children
        .map(completed_local_total_files)
        .ok_or_else(|| anyhow::anyhow!("local complete state missing collection children"))
}

/// Reconnect and request only ranges still missing from the local store on each retry.
async fn execute_download_with_retries(
    context: &ReceiveContext,
    plan: &DownloadPlan,
    emitter: &TransferEventEmitter,
) -> anyhow::Result<()> {
    let mut last_error = None;
    let mut reporter = ReceiverProgressReporter::new(emitter.clone(), plan.transfer_total);
    reporter.emit_initial_progress();
    for attempt in 1..=context.retry_policy.download_retry_limit {
        let local = context
            .db
            .remote()
            .local(context.hash_and_format())
            .await
            .context("inspect local download state")
            .map_err(|error| {
                receive_failure(
                    error,
                    TransferErrorCode::Filesystem,
                    TransferPhase::Transferring,
                    false,
                    "unable to inspect receiver storage",
                )
            })?;
        if local.is_complete() {
            reporter.emit_completed_progress();
            return Ok(());
        }

        let result =
            execute_download_attempt(context, local.missing(), local.local_bytes(), &mut reporter)
                .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                tracing::warn!(error = %error, attempt, "blob download attempt failed");
                last_error = Some(error);
                if attempt < context.retry_policy.download_retry_limit {
                    tokio::time::sleep(download_backoff(attempt, context.retry_policy)).await;
                }
            }
        }
    }

    Err(last_error.unwrap_or_else(|| {
        receive_failure(
            anyhow::anyhow!("download failed without an error"),
            TransferErrorCode::Internal,
            TransferPhase::Transferring,
            false,
            "download failed",
        )
    }))
}

/// Run one download attempt using a fresh connection and the currently missing ranges.
async fn execute_download_attempt(
    context: &ReceiveContext,
    missing: iroh_blobs::protocol::GetRequest,
    already_downloaded: u64,
    reporter: &mut ReceiverProgressReporter,
) -> anyhow::Result<()> {
    let connection = await_receive_phase(
        context.retry_policy.connect_timeout(),
        TransferPhase::Connecting,
        "connect to sender for blob download",
        async {
            context
                .endpoint
                .connect(context.addr.clone(), iroh_blobs::protocol::ALPN)
                .await
                .context("connect to sender for blob download")
        },
    )
    .await
    .map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::ConnectionFailed,
            TransferPhase::Connecting,
            true,
            "unable to connect to the sender",
        )
    })?;
    let get = context.db.remote().execute_get(connection, missing);
    let mut stream = get.stream();
    process_get_stream_with_reporter(
        &mut stream,
        already_downloaded,
        reporter,
        context.retry_policy.download_idle_timeout(),
    )
    .await
    .context("download blob stream")
    .map_err(|error| {
        receive_protocol_failure(
            error,
            TransferPhase::Transferring,
            "data transfer was interrupted",
        )
    })
}

fn collect_file_names(collection: &Collection) -> Vec<String> {
    collection
        .iter()
        .map(|(name, _hash)| name.to_string())
        .collect()
}

fn resolve_root_item_path(output_dir: &Path, collection: &Collection) -> anyhow::Result<PathBuf> {
    let root = collection_root_name(collection)?;
    Ok(output_dir.join(root))
}

fn resolve_output_dir(output_dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let resolved = match output_dir {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    Ok(resolved)
}

fn size_fetch_backoff(attempt: u32, retry_policy: ReceiveRetryPolicy) -> std::time::Duration {
    std::time::Duration::from_millis(retry_policy.size_fetch_backoff_ms * u64::from(attempt))
}

fn download_backoff(attempt: u32, retry_policy: ReceiveRetryPolicy) -> std::time::Duration {
    std::time::Duration::from_millis(
        retry_policy
            .download_retry_backoff_ms
            .saturating_mul(u64::from(attempt)),
    )
}

/// Await one receive phase, applying an optional user-provided timeout.
///
/// The caller decides which retry loop handles the returned error, so timeouts
/// reuse the same cleanup and retry behavior as connection or protocol failures.
async fn await_receive_phase<T, F>(
    timeout: Option<Duration>,
    transfer_phase: TransferPhase,
    phase: &'static str,
    future: F,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    match timeout {
        Some(timeout) => tokio::time::timeout(timeout, future).await.map_err(|_| {
            receive_failure(
                anyhow::anyhow!("{phase} timed out after {} ms", timeout.as_millis()),
                TransferErrorCode::Timeout,
                transfer_phase,
                true,
                timeout_event_message(transfer_phase),
            )
        })?,
        None => future.await,
    }
}

const fn timeout_event_message(phase: TransferPhase) -> &'static str {
    match phase {
        TransferPhase::Connecting => "connection timed out",
        TransferPhase::Metadata => "metadata request timed out",
        TransferPhase::Transferring => "download stalled without progress",
        TransferPhase::Preparing | TransferPhase::Exporting | TransferPhase::Finalizing => {
            "receive operation timed out"
        }
    }
}

fn finalize_cleanup(
    shutdown_result: anyhow::Result<()>,
    cleanup_result: anyhow::Result<()>,
) -> anyhow::Result<()> {
    if let Err(error) = cleanup_result {
        tracing::warn!(error = %error, "failed to clean temporary receive dir");
    }
    shutdown_result
}

/// 将 `GetError` 打印到日志并原样返回，便于上层处理。
fn show_get_error(e: GetError) -> GetError {
    log_get_error(&e);
    e
}

fn log_get_error(e: &GetError) {
    match e {
        GetError::InitialNext { .. }
        | GetError::ConnectedNext { .. }
        | GetError::AtBlobHeaderNext { .. } => {
            log_get_error_connection(e);
        }
        GetError::Decode { .. } | GetError::IrpcSend { .. } => {
            log_get_error_decode_or_irpc(e);
        }
        GetError::AtClosingNext { .. }
        | GetError::BadRequest { .. }
        | GetError::LocalFailure { .. } => {
            log_get_error_misc(e);
        }
    }
}

fn log_get_error_connection(e: &GetError) {
    match e {
        GetError::InitialNext { source, .. } => {
            tracing::error!("initial connection error: {source}")
        }
        GetError::ConnectedNext { source, .. } => tracing::error!("connected error: {source}"),
        GetError::AtBlobHeaderNext { source, .. } => {
            tracing::error!("reading blob header error: {source}")
        }
        _ => {}
    }
}

fn log_get_error_decode_or_irpc(e: &GetError) {
    match e {
        GetError::Decode { source, .. } => tracing::error!("decoding error: {source}"),
        GetError::IrpcSend { source, .. } => tracing::error!("error sending over irpc: {source}"),
        _ => {}
    }
}

fn log_get_error_misc(e: &GetError) {
    match e {
        GetError::AtClosingNext { source, .. } => tracing::error!("error at closing: {source}"),
        GetError::BadRequest { .. } => tracing::error!("bad request"),
        GetError::LocalFailure { source, .. } => tracing::error!("local failure {source:?}"),
        _ => {}
    }
}

/// 根据集合内的名称生成导出路径，同时验证每个路径组件的合法性。
fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if root.exists() {
        anyhow::ensure!(
            root.is_dir(),
            "output root {} is not a directory",
            root.display()
        );
    }
    std::fs::create_dir_all(root)?;
    let canonical_root = root.canonicalize()?;
    let parts = name.split('/');
    let mut path = root.to_path_buf();
    for part in parts {
        validate_path_component(part)?;
        path.push(part);
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid export target"))?;
    let canonical_existing_parent = canonicalize_existing_parent(parent)?;
    anyhow::ensure!(
        canonical_existing_parent.starts_with(&canonical_root),
        "final path must be within the root directory"
    );
    std::fs::create_dir_all(parent)?;

    let canonical_parent = parent.canonicalize()?;
    anyhow::ensure!(
        canonical_parent.starts_with(&canonical_root),
        "final path must be within the root directory"
    );

    ensure_export_target_is_absent(&path)?;

    Ok(path)
}

/// Refuse an existing staging target before the blob store writes to it.
///
/// `symlink_metadata` sees dangling symlinks and, on case-insensitive file systems,
/// also catches differently cased names that would otherwise overwrite an earlier entry.
fn ensure_export_target_is_absent(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("export target must not be a symbolic link")
        }
        Ok(_) => anyhow::bail!("export target {} already exists", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Resolve the nearest existing ancestor so symlink escapes are rejected before
/// creating any missing export directories outside the configured root.
fn canonicalize_existing_parent(path: &Path) -> anyhow::Result<PathBuf> {
    let mut current = path;
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(_) => return Ok(current.canonicalize()?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = current
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("invalid export target"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

// Helper: prepare endpoint, temp dir and FsStore
async fn prepare_env(
    ticket: &BlobTicket,
    options: &ReceiveOptions,
) -> anyhow::Result<(Endpoint, PathBuf, Store, Option<ReceiveCacheLease>)> {
    let mut builder = base_endpoint_builder(options, vec![]).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::InvalidInput,
            TransferPhase::Preparing,
            false,
            "invalid receive networking options",
        )
    })?;

    if ticket.addr().relay_urls().next().is_none() && ticket.addr().ip_addrs().next().is_none() {
        builder = builder.address_lookup(DnsAddressLookup::n0_dns());
    }
    let endpoint = builder.bind().await.map_err(|error| {
        receive_failure(
            error.into(),
            TransferErrorCode::ConnectionFailed,
            TransferPhase::Connecting,
            true,
            "unable to initialize receiver networking",
        )
    })?;

    let storage_result = options.receive_cache.as_ref().map_or_else(
        || {
            unique_temp_dir(&format!(
                "{RECEIVE_TEMP_DIR_PREFIX}{}-",
                ticket.hash().to_hex()
            ))
            .map(|path| (path, None))
        },
        |cache| {
            ReceiveCacheLease::open(cache, ticket.hash_and_format()).map(|lease| {
                let path = lease.entry_dir().to_path_buf();
                (path, Some(lease))
            })
        },
    );
    let (iroh_data_dir, cache_lease) = match storage_result {
        Ok(storage) => storage,
        Err(error) => {
            close_receive_endpoint(&endpoint).await;
            return Err(receive_failure(
                error,
                TransferErrorCode::Filesystem,
                TransferPhase::Preparing,
                false,
                "unable to create receiver storage",
            ));
        }
    };
    let db = match load_fs_store(&iroh_data_dir).await {
        Ok(db) => db,
        Err(error) => {
            close_receive_endpoint(&endpoint).await;
            let cleanup_result = match cache_lease {
                Some(lease) => lease.preserve(),
                None => remove_temp_receive_dir(&iroh_data_dir).await,
            };
            if let Err(cleanup_error) = cleanup_result {
                tracing::warn!(
                    error = %cleanup_error,
                    "failed to finalize receiver storage after open error"
                );
            }
            return Err(receive_failure(
                error,
                TransferErrorCode::Filesystem,
                TransferPhase::Preparing,
                false,
                "unable to open receiver storage",
            ));
        }
    };
    Ok((endpoint, iroh_data_dir, db.into(), cache_lease))
}

/// Fetch remote collection sizes with a fresh connection for each retry.
///
/// Reconnecting inside the attempt loop means an initial connection failure is
/// retried too, instead of skipping the retry policy before any request starts.
async fn get_sizes_with_retries(
    endpoint: &Endpoint,
    addr: &iroh::EndpointAddr,
    hash: &iroh_blobs::Hash,
    retry_policy: ReceiveRetryPolicy,
) -> anyhow::Result<(iroh_blobs::hashseq::HashSeq, StdArc<[u64]>)> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=retry_policy.size_fetch_retry_limit {
        let connection = match await_receive_phase(
            retry_policy.connect_timeout(),
            TransferPhase::Connecting,
            "connect to sender for collection metadata",
            async {
                endpoint
                    .connect(addr.clone(), iroh_blobs::protocol::ALPN)
                    .await
                    .context("connect to sender for collection metadata")
            },
        )
        .await
        {
            Ok(connection) => connection,
            Err(error) => {
                let error = receive_failure(
                    error,
                    TransferErrorCode::ConnectionFailed,
                    TransferPhase::Connecting,
                    true,
                    "unable to connect to the sender",
                );
                tracing::error!("Attempt {attempt} to connect for sizes failed: {error}");
                last_err = Some(error);
                if attempt < retry_policy.size_fetch_retry_limit {
                    tokio::time::sleep(size_fetch_backoff(attempt, retry_policy)).await;
                }
                continue;
            }
        };

        match await_receive_phase(
            retry_policy.metadata_timeout(),
            TransferPhase::Metadata,
            "fetch collection metadata",
            async {
                get_hash_seq_and_sizes(&connection, hash, retry_policy.size_fetch_chunk_size, None)
                    .await
                    .map_err(show_get_error)
                    .map_err(anyhow::Error::from)
            },
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) => {
                let error = receive_protocol_failure(
                    error,
                    TransferPhase::Metadata,
                    "collection metadata transfer was interrupted",
                );
                tracing::error!("Attempt {attempt} to get sizes failed: {error:?}");
                last_err = Some(error);
                if attempt < retry_policy.size_fetch_retry_limit {
                    tokio::time::sleep(size_fetch_backoff(attempt, retry_policy)).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        receive_failure(
            anyhow::anyhow!("unknown error getting sizes"),
            TransferErrorCode::Internal,
            TransferPhase::Metadata,
            false,
            "unable to fetch collection metadata",
        )
    }))
}

/// Consume one get stream, retaining one reporter across retries and leaving failure reporting to the caller.
async fn process_get_stream_with_reporter<S>(
    stream: &mut S,
    already_downloaded: u64,
    reporter: &mut ReceiverProgressReporter,
    idle_timeout: Option<Duration>,
) -> anyhow::Result<()>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    let mut seen_done = false;
    while let Some(item) = next_get_progress_item(stream, idle_timeout).await? {
        trace!("got item {item:?}");
        match item {
            GetProgressItem::Progress(offset) => {
                reporter.on_progress(already_downloaded.saturating_add(offset));
            }
            GetProgressItem::Done(value) => {
                let _stats = value;
                reporter.emit_completed_progress();
                seen_done = true;
                break;
            }
            GetProgressItem::Error(cause) => {
                tracing::error!("Download error: {:?}", cause);
                let error = show_get_error(cause);
                anyhow::bail!(error);
            }
        }
    }
    anyhow::ensure!(seen_done, "download stream ended before completion");
    Ok(())
}

/// Wait for the next stream update, restarting the idle timer after each update.
async fn next_get_progress_item<S>(
    stream: &mut S,
    idle_timeout: Option<Duration>,
) -> anyhow::Result<Option<GetProgressItem>>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    match idle_timeout {
        Some(timeout) => tokio::time::timeout(timeout, stream.next())
            .await
            .map_err(|_| {
                receive_failure(
                    anyhow::anyhow!(
                        "download stream timed out after {} ms without progress",
                        timeout.as_millis()
                    ),
                    TransferErrorCode::Timeout,
                    TransferPhase::Transferring,
                    true,
                    "download stalled without progress",
                )
            }),
        None => Ok(stream.next().await),
    }
}

/// 验证单个路径组件是否合法（不应包含分隔符 `/`）。
fn validate_path_component(component: &str) -> anyhow::Result<()> {
    // Check for empty components
    anyhow::ensure!(!component.is_empty(), "path component cannot be empty");

    // Check for path separators
    anyhow::ensure!(
        !component.contains('/') && !component.contains('\\'),
        "path components must not contain path separators"
    );

    // Check for path traversal attempts
    anyhow::ensure!(component != "..", "path traversal not allowed: '..'");
    anyhow::ensure!(component != ".", "relative path not allowed: '.'");

    // Check for absolute paths
    anyhow::ensure!(
        !component.starts_with('/'),
        "absolute path components not allowed"
    );

    // Optional: Check for hidden files (starting with '.')
    // Uncomment if you want to restrict hidden files
    // anyhow::ensure!(
    //     !component.starts_with('.') || component.len() == 1,
    //     "hidden files not allowed"
    // );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DownloadPlan, ReceiveRetryPolicy, await_receive_phase, base_endpoint_builder,
        close_receive_endpoint, collection_root_name, commit_staged_export,
        completed_local_total_files, completed_local_total_files_from_children, create_staging_dir,
        download_backoff, emit_receive_failed, finalize_cleanup, finalize_failed_receive,
        get_export_path, move_staged_root_without_replacing, process_export_stream,
        process_get_stream_with_reporter, receive, receive_failed_message, receive_failure,
        resolve_output_dir, size_fetch_backoff, validate_path_component, wait_for_cancellation,
    };
    use crate::core::events::{
        EventEmitter, Role, TransferErrorCode, TransferEvent, TransferEventData, TransferPhase,
        classified_transfer_error,
    };
    use crate::core::options::{ReceiveOptions, RelayModeOption};
    use crate::core::progress::{ReceiverProgressReporter, TransferEventEmitter};
    use iroh_blobs::Hash;
    use iroh_blobs::api::{blobs::ExportProgressItem, remote::GetProgressItem};
    use n0_future::stream;
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex};
    use std::time::Duration;

    #[derive(Default)]
    struct RecordingEmitter {
        events: StdMutex<Vec<TransferEvent>>,
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

    #[tokio::test]
    async fn invalid_ticket_emits_structured_invalid_input_failure() {
        let emitter = Arc::new(RecordingEmitter::default());
        let error = receive(
            "not-a-ticket".to_owned(),
            ReceiveOptions::default(),
            Some(emitter.clone()),
        )
        .await
        .expect_err("invalid ticket should fail");

        assert!(!error.to_string().is_empty());
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

    fn receiver_reporter(
        app_handle: crate::core::events::AppHandle,
        total: u64,
    ) -> ReceiverProgressReporter {
        let emitter = TransferEventEmitter::new(app_handle, Role::Receiver);
        emitter.emit_started(TransferPhase::Transferring);
        ReceiverProgressReporter::new(emitter, total)
    }

    #[test]
    fn validate_path_component_accepts_normal_name() {
        validate_path_component("report.txt").expect("regular filename should be allowed");
    }

    #[test]
    fn validate_path_component_rejects_empty_name() {
        let err = validate_path_component("").expect_err("empty component should fail");
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[test]
    fn validate_path_component_rejects_path_traversal() {
        let err = validate_path_component("..").expect_err("parent traversal should fail");
        assert!(err.to_string().contains("path traversal"));
    }

    #[test]
    fn validate_path_component_rejects_path_separator() {
        let err = validate_path_component("dir/file").expect_err("separator should fail");
        assert!(err.to_string().contains("must not contain path separators"));
    }

    #[test]
    fn get_export_path_joins_nested_relative_path() {
        let root = Path::new("downloads");
        let export_path = get_export_path(root, "dir/subdir/file.bin")
            .expect("nested relative path should be accepted");
        assert_eq!(
            export_path,
            root.join("dir").join("subdir").join("file.bin")
        );
    }

    #[test]
    fn get_export_path_rejects_traversal_component() {
        let root = Path::new("downloads");
        let err = get_export_path(root, "../secret.txt").expect_err("traversal should fail");
        assert!(err.to_string().contains("path traversal"));
    }

    #[test]
    fn get_export_path_rejects_empty_component() {
        let root = Path::new("downloads");
        let err = get_export_path(root, "dir//file.txt").expect_err("empty component should fail");
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[test]
    fn get_export_path_rejects_absolute_like_name() {
        let root = tempfile::tempdir()
            .expect("temp dir")
            .path()
            .join("downloads");
        let err = get_export_path(&root, "/etc/passwd")
            .expect_err("absolute-style export name should fail");
        assert!(err.to_string().contains("cannot be empty"));
    }

    #[test]
    fn get_export_path_rejects_when_root_is_a_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root_file = temp_dir.path().join("not-a-dir");
        std::fs::write(&root_file, b"content").expect("write root file");

        let err =
            get_export_path(&root_file, "dir/file.txt").expect_err("file root should be rejected");
        assert!(err.to_string().contains("is not a directory"));
    }

    #[test]
    fn get_export_path_rejects_existing_regular_file() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("stage");
        let target = get_export_path(&root, "already-exported.txt")
            .expect("first staged target should be available");
        std::fs::write(&target, b"first entry").expect("write staged target");

        let error = get_export_path(&root, "already-exported.txt")
            .expect_err("an existing staging target must not be overwritten");

        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            std::fs::read(target).expect("original file remains"),
            b"first entry"
        );
    }

    #[cfg(windows)]
    #[test]
    fn get_export_path_rejects_case_insensitive_staging_collision() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("stage");
        let first_target =
            get_export_path(&root, "A.txt").expect("first staged target should be available");
        std::fs::write(&first_target, b"first entry").expect("write staged target");

        let error = get_export_path(&root, "a.txt")
            .expect_err("case-insensitive aliases must not overwrite staged data");

        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            std::fs::read(first_target).expect("original file remains"),
            b"first entry"
        );
    }

    #[test]
    fn commit_staged_export_moves_complete_root_into_place() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let output_dir = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&output_dir).expect("output directory");
        let staging_dir = create_staging_dir(&output_dir).expect("staging directory");
        let staged_root = staging_dir.join("root");
        std::fs::create_dir_all(&staged_root).expect("staged root");
        std::fs::write(staged_root.join("file.txt"), b"complete").expect("staged file");

        commit_staged_export(&staging_dir, &output_dir, "root")
            .expect("staged export should commit");

        assert_eq!(
            std::fs::read(output_dir.join("root/file.txt")).expect("committed file"),
            b"complete"
        );
        assert!(!staging_dir.exists());
    }

    #[test]
    fn commit_staged_export_succeeds_when_staging_cleanup_is_not_empty() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let output_dir = temp_dir.path().join("downloads");
        std::fs::create_dir_all(&output_dir).expect("output directory");
        let staging_dir = create_staging_dir(&output_dir).expect("staging directory");
        let staged_root = staging_dir.join("root");
        std::fs::create_dir_all(&staged_root).expect("staged root");
        std::fs::write(staged_root.join("file.txt"), b"complete").expect("staged file");
        std::fs::write(staging_dir.join("leftover.txt"), b"cleanup only").expect("leftover");

        commit_staged_export(&staging_dir, &output_dir, "root")
            .expect("the committed root must not be reported as a failure");

        assert_eq!(
            std::fs::read(output_dir.join("root/file.txt")).expect("committed file"),
            b"complete"
        );
        assert!(!staging_dir.exists(), "best-effort cleanup removes staging");
    }

    #[test]
    fn commit_staged_export_classifies_existing_target() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let output_dir = temp_dir.path().join("downloads");
        std::fs::create_dir_all(output_dir.join("root")).expect("existing target");
        let staging_dir = create_staging_dir(&output_dir).expect("staging directory");
        std::fs::create_dir_all(staging_dir.join("root")).expect("staged root");

        let error = commit_staged_export(&staging_dir, &output_dir, "root")
            .expect_err("existing target should fail");
        let details = classified_transfer_error(&error).expect("structured target conflict");

        assert_eq!(details.code, TransferErrorCode::TargetConflict);
        assert_eq!(details.phase, TransferPhase::Exporting);
        assert!(!details.retryable);
    }

    #[test]
    fn move_staged_root_does_not_replace_existing_target() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let source = temp_dir.path().join("staged-file");
        let target = temp_dir.path().join("existing-file");
        std::fs::write(&source, b"complete").expect("staged file");
        std::fs::write(&target, b"existing").expect("existing file");

        let error = move_staged_root_without_replacing(&source, &target)
            .expect_err("native move must refuse to replace an existing target");

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&source).expect("staged file remains"),
            b"complete"
        );
        assert_eq!(
            std::fs::read(&target).expect("existing file remains"),
            b"existing"
        );
    }

    #[test]
    fn collection_root_name_rejects_multiple_top_level_roots() {
        let collection = [
            ("first/file.txt", Hash::new(b"first")),
            ("second/file.txt", Hash::new(b"second")),
        ]
        .into_iter()
        .collect();

        let error = collection_root_name(&collection)
            .expect_err("multiple roots cannot be committed atomically");

        assert!(error.to_string().contains("multiple top-level roots"));
    }

    #[cfg(unix)]
    #[test]
    fn get_export_path_rejects_symlinked_parent_outside_root() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("downloads");
        let outside = temp_dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("create outside directory");
        std::fs::create_dir_all(&root).expect("create output directory");
        symlink(&outside, root.join("nested")).expect("create parent symlink");

        let err = get_export_path(&root, "nested/file.txt")
            .expect_err("symlinked parent outside root should be rejected");
        assert!(err.to_string().contains("within the root"));
        assert!(
            !outside.join("file.txt").exists(),
            "outside target must not be created"
        );
    }

    #[cfg(unix)]
    #[test]
    fn get_export_path_rejects_dangling_target_symlink() {
        use std::os::unix::fs::symlink;

        let temp_dir = tempfile::tempdir().expect("temp dir");
        let root = temp_dir.path().join("downloads");
        let outside_target = temp_dir.path().join("outside").join("file.txt");
        std::fs::create_dir_all(root.parent().expect("root parent")).expect("create temp root");
        std::fs::create_dir_all(&root).expect("create output directory");
        symlink(&outside_target, root.join("file.txt")).expect("create dangling target symlink");

        let err = get_export_path(&root, "file.txt")
            .expect_err("dangling target symlink should be rejected");
        assert!(err.to_string().contains("symbolic link"));
        assert!(
            !outside_target.exists(),
            "outside target must remain absent"
        );
    }

    #[test]
    fn completed_local_total_files_handles_empty_collection() {
        assert_eq!(completed_local_total_files(0), 0);
        assert_eq!(completed_local_total_files(1), 0);
        assert_eq!(completed_local_total_files(3), 2);
    }

    #[test]
    fn download_plan_separates_collection_metadata_from_file_payload() {
        let hash_seq = [
            Hash::new(b"metadata"),
            Hash::new(b"first file"),
            Hash::new(b"second file"),
        ]
        .into_iter()
        .collect();
        let plan = DownloadPlan::from_hash_seq_and_sizes(&hash_seq, &[18, 100, 200]);

        assert_eq!(plan.total_files, 2);
        assert_eq!(plan.payload_size, 300);
        assert_eq!(plan.transfer_total, 414);
    }

    #[test]
    fn completed_local_total_files_from_children_rejects_missing_children() {
        let err = completed_local_total_files_from_children(None)
            .expect_err("missing children should be rejected");
        assert!(err.to_string().contains("missing collection children"));
    }

    #[test]
    fn receive_failed_message_wraps_error_with_prefix() {
        let message = receive_failed_message(&anyhow::anyhow!("boom"));
        assert_eq!(message, "error: boom");
    }

    #[test]
    fn receive_failed_message_preserves_failure_stage_context() {
        let error = anyhow::anyhow!("connection refused").context("fetch remote collection sizes");
        let message = receive_failed_message(&error);
        assert_eq!(
            message,
            "error: fetch remote collection sizes: connection refused"
        );
    }

    #[tokio::test]
    async fn cancellation_wait_returns_after_signal() {
        let (sender, receiver) = tokio::sync::watch::channel(false);
        let wait = tokio::spawn(wait_for_cancellation(Some(receiver)));
        sender.send(true).expect("send cancellation signal");
        tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .expect("cancellation should wake the receive task")
            .expect("cancellation wait task should finish");
    }

    #[test]
    fn retryable_get_stream_error_does_not_emit_terminal_event() {
        let emitter = Arc::new(RecordingEmitter::default());
        let app_handle: crate::core::events::AppHandle = Some(emitter.clone());

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let mut s = stream::empty::<GetProgressItem>();
            let mut reporter = receiver_reporter(app_handle, 12);
            reporter.emit_initial_progress();
            let err = process_get_stream_with_reporter(&mut s, 0, &mut reporter, None)
                .await
                .expect_err("stream ending early should fail");
            assert!(err.to_string().contains("ended before completion"));
        });

        let events = emitter.events();
        assert!(matches!(
            events.get(1),
            Some(event)
                if event.role == Role::Receiver
                    && matches!(
                        &event.event,
                        TransferEventData::Progress {
                            processed: 0,
                            total: 12,
                            ..
                        }
                    )
        ));
        assert!(!events.iter().any(|event| event.event.is_terminal()));
    }

    #[test]
    fn process_export_stream_rejects_early_end() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let mut s = stream::empty::<ExportProgressItem>();
            let err = process_export_stream(&mut s, "report.txt")
                .await
                .expect_err("stream ending early should fail");
            assert!(err.to_string().contains("ended before completion"));
        });
    }

    #[test]
    fn process_export_stream_accepts_done() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let mut s = stream::iter([ExportProgressItem::Done]);
            process_export_stream(&mut s, "report.txt")
                .await
                .expect("done item should complete export");
        });
    }

    #[test]
    fn emit_receive_failed_emits_receiver_failed_event() {
        let emitter = Arc::new(RecordingEmitter::default());
        let app_handle: crate::core::events::AppHandle = Some(emitter.clone());
        let session = TransferEventEmitter::new(app_handle, Role::Receiver);
        session.emit_started(TransferPhase::Connecting);
        let failure = receive_failure(
            anyhow::anyhow!("connection refused"),
            TransferErrorCode::ConnectionFailed,
            TransferPhase::Connecting,
            true,
            "unable to connect to the sender",
        );

        emit_receive_failed(&session, &failure);

        let events = emitter.events();
        assert_eq!(events.len(), 2);
        match &events[1] {
            TransferEvent {
                role,
                event: TransferEventData::Failed { error },
                ..
            } => {
                assert_eq!(*role, Role::Receiver);
                assert_eq!(error.code, TransferErrorCode::ConnectionFailed);
                assert_eq!(error.phase, TransferPhase::Connecting);
                assert!(error.retryable);
                assert_eq!(error.message, "unable to connect to the sender");
            }
            other => panic!("expected failed event, got {other:?}"),
        }
    }

    #[test]
    fn resolve_output_dir_uses_explicit_value() {
        let dir = Path::new("explicit-dir").to_path_buf();
        let resolved = resolve_output_dir(Some(dir.clone())).expect("explicit output should pass");
        assert_eq!(resolved, dir);
    }

    #[test]
    fn resolve_output_dir_defaults_to_current_directory() {
        let expected = std::env::current_dir().expect("current dir");
        let resolved = resolve_output_dir(None).expect("default output should resolve");
        assert_eq!(resolved, expected);
    }

    #[test]
    fn size_fetch_backoff_scales_by_attempt() {
        let policy = ReceiveRetryPolicy {
            size_fetch_backoff_ms: 125,
            ..Default::default()
        };

        assert_eq!(size_fetch_backoff(0, policy), std::time::Duration::ZERO);
        assert_eq!(
            size_fetch_backoff(2, policy),
            std::time::Duration::from_millis(250)
        );
    }

    #[test]
    fn download_backoff_scales_by_attempt() {
        let policy = ReceiveRetryPolicy {
            download_retry_backoff_ms: 75,
            ..Default::default()
        };

        assert_eq!(download_backoff(0, policy), std::time::Duration::ZERO);
        assert_eq!(
            download_backoff(3, policy),
            std::time::Duration::from_millis(225)
        );
    }

    #[tokio::test]
    async fn close_receive_endpoint_marks_endpoint_closed() {
        let options = ReceiveOptions {
            relay_mode: RelayModeOption::Disabled,
            ..Default::default()
        };
        let endpoint = base_endpoint_builder(&options, vec![])
            .expect("endpoint builder")
            .bind()
            .await
            .expect("endpoint should bind");

        close_receive_endpoint(&endpoint).await;

        assert!(endpoint.is_closed());
    }

    #[test]
    fn finalize_failed_receive_preserves_primary_error_when_cleanup_fails() {
        let err = finalize_failed_receive(
            anyhow::anyhow!("primary failure"),
            Err(anyhow::anyhow!("cleanup failure")),
        );
        assert!(err.to_string().contains("primary failure"));
    }

    #[test]
    fn finalize_cleanup_returns_shutdown_error_even_if_cleanup_fails() {
        let shutdown_error = anyhow::anyhow!("shutdown failed");
        let cleanup_error = anyhow::anyhow!("cleanup failed");
        let err = finalize_cleanup(Err(shutdown_error), Err(cleanup_error))
            .expect_err("shutdown error should be preserved");
        assert!(err.to_string().contains("shutdown failed"));
    }

    #[test]
    fn finalize_cleanup_succeeds_if_shutdown_succeeds() {
        finalize_cleanup(Ok(()), Err(anyhow::anyhow!("cleanup failed")))
            .expect("cleanup failures should not fail operation");
    }

    #[tokio::test]
    async fn process_get_stream_errors_if_stream_ends_before_done() {
        let mut s = stream::empty::<GetProgressItem>();
        let mut reporter = receiver_reporter(None, 0);
        let err = process_get_stream_with_reporter(&mut s, 0, &mut reporter, None)
            .await
            .expect_err("stream ending early should fail");
        assert!(err.to_string().contains("ended before completion"));
    }

    #[tokio::test]
    async fn receive_phase_timeout_reports_the_timed_out_operation() {
        let error = await_receive_phase(
            Some(Duration::from_millis(1)),
            TransferPhase::Connecting,
            "connect to test sender",
            std::future::pending::<anyhow::Result<()>>(),
        )
        .await
        .expect_err("pending operation should time out");

        assert!(
            error
                .to_string()
                .contains("connect to test sender timed out")
        );
        let details = classified_transfer_error(&error).expect("structured timeout");
        assert_eq!(details.code, TransferErrorCode::Timeout);
        assert_eq!(details.phase, TransferPhase::Connecting);
        assert!(details.retryable);
    }

    #[tokio::test]
    async fn process_get_stream_times_out_after_idle_period() {
        let mut stream = stream::pending::<GetProgressItem>();
        let mut reporter = receiver_reporter(None, 0);

        let error = process_get_stream_with_reporter(
            &mut stream,
            0,
            &mut reporter,
            Some(Duration::from_millis(1)),
        )
        .await
        .expect_err("idle stream should time out");

        assert!(error.to_string().contains("without progress"));
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn process_get_stream_resets_idle_timeout_after_progress() {
        let mut stream = Box::pin(stream::unfold(0u8, |step| async move {
            match step {
                0 => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    Some((GetProgressItem::Progress(5), 1))
                }
                1 => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    Some((GetProgressItem::Done(Default::default()), 2))
                }
                _ => None,
            }
        }));
        let mut reporter = receiver_reporter(None, 10);

        process_get_stream_with_reporter(
            &mut stream,
            0,
            &mut reporter,
            Some(Duration::from_millis(8)),
        )
        .await
        .expect("each stream item should reset the idle timer");
    }
}
