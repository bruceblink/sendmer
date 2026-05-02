//! 接收端功能：根据票据连接远端并导出数据到本地目录。
//!
//! 主要导出 `download`，它负责建立连接、跟踪进度并将文件导出到目标目录。

use crate::core::endpoint::base_endpoint_builder;
use crate::core::events::AppHandle;
use crate::core::options::{ReceiveOptions, ReceiveRetryPolicy};
use crate::core::progress::{ReceiverProgressReporter, TransferEventEmitter};
use crate::core::results::ReceiveResult;
use crate::core::storage::{load_fs_store, unique_temp_dir};
use iroh::{Endpoint, discovery::dns::DnsDiscovery};
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
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc as StdArc;
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
    let ticket = BlobTicket::from_str(&ticket_str)?;
    info!(
        hash = %ticket.hash(),
        relay_addrs = ticket.addr().relay_urls().count(),
        ip_addrs = ticket.addr().ip_addrs().count(),
        "starting receive"
    );
    let context = ReceiveContext::prepare(ticket, &options).await?;
    let output_dir = resolve_output_dir(options.output_dir)?;

    let artifacts = select! {
        x = receive_once(&context, &output_dir, app_handle.clone()) => match x {
            Ok(artifacts) => artifacts,
            Err(error) => {
                tracing::error!(error = %error, "download operation failed");
                let message = receive_failed_message(&error);
                emit_receive_failed(&app_handle, message.clone());
                let error = finalize_failed_receive(
                    anyhow::anyhow!(message),
                    cleanup_failed_receive(&context).await,
                );
                return Err(error);
            }
        },
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!("operation cancelled by user");
            let message = receive_cancelled_message();
            emit_receive_failed(&app_handle, message);
            let error = finalize_failed_receive(
                anyhow::anyhow!(message),
                cleanup_failed_receive(&context).await,
            );
            return Err(error);
        }
    };

    let result = finish_receive(&context, artifacts).await?;
    info!(output = %result.file_path.display(), message = %result.message, "receive completed");
    Ok(result)
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

struct ReceiveContext {
    ticket: BlobTicket,
    addr: iroh::EndpointAddr,
    endpoint: Endpoint,
    iroh_data_dir: PathBuf,
    db: Store,
    retry_policy: ReceiveRetryPolicy,
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
}

impl ReceiveContext {
    async fn prepare(ticket: BlobTicket, options: &ReceiveOptions) -> anyhow::Result<Self> {
        let addr = ticket.addr().clone();
        let (endpoint, iroh_data_dir, db) = prepare_env(&ticket, options).await?;
        Ok(Self {
            ticket,
            addr,
            endpoint,
            iroh_data_dir,
            db,
            retry_policy: options.retry_policy,
        })
    }

    fn hash_and_format(&self) -> iroh_blobs::HashAndFormat {
        self.ticket.hash_and_format()
    }

    async fn load_collection(&self) -> anyhow::Result<Collection> {
        Collection::load(self.hash_and_format().hash, &self.db).await
    }
}

impl DownloadPlan {
    fn from_sizes(sizes: &[u64]) -> Self {
        Self {
            total_files: sizes.len().saturating_sub(1) as u64,
            payload_size: sizes.iter().skip(1).copied().sum::<u64>(),
        }
    }
}

async fn receive_once(
    context: &ReceiveContext,
    output_dir: &Path,
    app_handle: AppHandle,
) -> anyhow::Result<ReceiveArtifacts> {
    trace!("load done!");

    let event_emitter =
        TransferEventEmitter::new(app_handle.clone(), crate::core::events::Role::Receiver);
    let download = download_missing_data(context, app_handle).await?;
    let collection = context.load_collection().await?;
    emit_collection_file_names(&event_emitter, &collection);
    let root_item_path = resolve_root_item_path(output_dir, &collection)?;
    export(&context.db, collection, output_dir).await?;
    event_emitter.emit_completed();

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

fn receive_failed_message(error: &anyhow::Error) -> String {
    format!("error: {error}")
}

fn receive_failed_message_from_get_error(error: &GetError) -> String {
    format!("error: {error}")
}

const fn receive_cancelled_message() -> &'static str {
    "Operation cancelled"
}

const fn receive_stream_ended_message() -> &'static str {
    "download stream ended before completion"
}

fn emit_receive_failed(app_handle: &AppHandle, message: impl Into<String>) {
    let emitter =
        TransferEventEmitter::new(app_handle.clone(), crate::core::events::Role::Receiver);
    emitter.emit_failed(message);
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

async fn cleanup_failed_receive(context: &ReceiveContext) -> anyhow::Result<()> {
    let shutdown_result = context.db.shutdown().await.map_err(anyhow::Error::from);
    let cleanup_result = remove_temp_receive_dir(&context.iroh_data_dir).await;
    finalize_cleanup(shutdown_result, cleanup_result)
}

async fn finish_receive(
    context: &ReceiveContext,
    artifacts: ReceiveArtifacts,
) -> anyhow::Result<ReceiveResult> {
    let shutdown_result = context.db.shutdown().await.map_err(anyhow::Error::from);
    let cleanup_result = remove_temp_receive_dir(&context.iroh_data_dir).await;
    finalize_cleanup(shutdown_result, cleanup_result)?;

    Ok(ReceiveResult {
        message: format!(
            "Downloaded {} files, {} bytes",
            artifacts.total_files, artifacts.payload_size
        ),
        file_path: artifacts.root_item_path,
    })
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
    app_handle: AppHandle,
) -> anyhow::Result<DownloadOutcome> {
    let emitter =
        TransferEventEmitter::new(app_handle.clone(), crate::core::events::Role::Receiver);
    let hash_and_format = context.hash_and_format();
    let local = context.db.remote().local(hash_and_format).await?;
    if local.is_complete() {
        let total_files = completed_local_total_files_from_children(local.children())?;
        emitter.emit_started();
        return Ok(DownloadOutcome {
            total_files,
            payload_size: 0,
        });
    }

    emitter.emit_started();
    let (_hash_seq, sizes) = get_sizes_with_retries(
        &context.endpoint,
        &context.addr,
        &context.ticket.hash(),
        context.retry_policy,
    )
    .await?;
    let plan = DownloadPlan::from_sizes(&sizes);
    execute_download(context, local.missing(), &plan, &app_handle).await?;

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

async fn execute_download(
    context: &ReceiveContext,
    missing: iroh_blobs::protocol::GetRequest,
    plan: &DownloadPlan,
    app_handle: &AppHandle,
) -> anyhow::Result<()> {
    let connection = context
        .endpoint
        .connect(context.addr.clone(), iroh_blobs::protocol::ALPN)
        .await?;
    let get = context.db.remote().execute_get(connection, missing);
    let mut stream = get.stream();
    process_get_stream(&mut stream, plan.payload_size, app_handle).await
}

fn collect_file_names(collection: &Collection) -> Vec<String> {
    collection
        .iter()
        .map(|(name, _hash)| name.to_string())
        .collect()
}

fn resolve_root_item_path(output_dir: &Path, collection: &Collection) -> anyhow::Result<PathBuf> {
    let mut names = collection.iter().map(|(name, _)| name);
    let Some(first_name) = names.next() else {
        anyhow::bail!("collection is empty")
    };

    let Some(first_root) = first_name.split('/').next().filter(|part| !part.is_empty()) else {
        anyhow::bail!("collection contains invalid entry name")
    };

    if names
        .filter_map(|name| name.split('/').next())
        .any(|root| root != first_root)
    {
        return get_export_path(output_dir, first_name);
    }

    get_export_path(output_dir, first_root)
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

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let canonical_parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid export target"))?
        .canonicalize()?;
    anyhow::ensure!(
        canonical_parent.starts_with(&canonical_root),
        "final path must be within the root directory"
    );

    Ok(path)
}

// Helper: prepare endpoint, temp dir and FsStore
async fn prepare_env(
    ticket: &BlobTicket,
    options: &ReceiveOptions,
) -> anyhow::Result<(Endpoint, PathBuf, Store)> {
    let mut builder = base_endpoint_builder(options, vec![])?;

    if ticket.addr().relay_urls().next().is_none() && ticket.addr().ip_addrs().next().is_none() {
        builder = builder.discovery(DnsDiscovery::n0_dns());
    }
    let endpoint = builder.bind().await?;

    let iroh_data_dir = unique_temp_dir(&format!(
        "{RECEIVE_TEMP_DIR_PREFIX}{}-",
        ticket.hash().to_hex()
    ))?;
    let db = load_fs_store(&iroh_data_dir).await?;
    Ok((endpoint, iroh_data_dir, db.into()))
}

// Helper: get sizes with retries and reconnects
async fn get_sizes_with_retries(
    endpoint: &Endpoint,
    addr: &iroh::EndpointAddr,
    hash: &iroh_blobs::Hash,
    retry_policy: ReceiveRetryPolicy,
) -> anyhow::Result<(iroh_blobs::hashseq::HashSeq, StdArc<[u64]>)> {
    let mut last_err: Option<GetError> = None;
    let mut connection = endpoint
        .connect(addr.clone(), iroh_blobs::protocol::ALPN)
        .await?;
    for attempt in 1..=retry_policy.size_fetch_retry_limit {
        match get_hash_seq_and_sizes(&connection, hash, retry_policy.size_fetch_chunk_size, None)
            .await
        {
            Ok(result) => return Ok(result),
            Err(e) => {
                tracing::error!("Attempt {attempt} to get sizes failed: {e:?}");
                last_err = Some(e);
                if attempt < retry_policy.size_fetch_retry_limit {
                    tokio::time::sleep(size_fetch_backoff(attempt, retry_policy)).await;
                    reconnect(endpoint, addr, &mut connection).await;
                }
            }
        }
    }

    if let Some(e) = last_err {
        tracing::error!("Failed to get sizes after retries: {:?}", e);
        tracing::error!("Error type: {}", std::any::type_name_of_val(&e));
        Err(show_get_error(e).into())
    } else {
        anyhow::bail!("unknown error getting sizes")
    }
}

async fn reconnect(
    endpoint: &Endpoint,
    addr: &iroh::EndpointAddr,
    connection: &mut iroh::endpoint::Connection,
) {
    match endpoint
        .connect(addr.clone(), iroh_blobs::protocol::ALPN)
        .await
    {
        Ok(new_connection) => *connection = new_connection,
        Err(conn_err) => tracing::error!("reconnect failed: {conn_err}"),
    }
}

// Helper: process a Get stream and emit progress events
async fn process_get_stream<S>(
    stream: &mut S,
    payload_size: u64,
    app_handle: &AppHandle,
) -> anyhow::Result<()>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    let mut reporter = ReceiverProgressReporter::new(app_handle.clone(), payload_size);
    reporter.emit_initial_progress();
    let mut seen_done = false;
    while let Some(item) = stream.next().await {
        trace!("got item {item:?}");
        match item {
            GetProgressItem::Progress(offset) => {
                reporter.on_progress(offset);
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
                reporter.emit_failed(receive_failed_message_from_get_error(&error));
                anyhow::bail!(error);
            }
        }
    }
    if !seen_done {
        reporter.emit_failed(receive_stream_ended_message());
    }
    anyhow::ensure!(seen_done, "download stream ended before completion");
    Ok(())
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
        completed_local_total_files, completed_local_total_files_from_children,
        emit_receive_failed, finalize_cleanup, finalize_failed_receive, get_export_path,
        process_get_stream, receive_failed_message, receive_stream_ended_message,
        resolve_output_dir, validate_path_component,
    };
    use crate::core::events::{EventEmitter, Role, TransferEvent};
    use iroh_blobs::api::remote::GetProgressItem;
    use n0_future::stream;
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex};

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
    fn completed_local_total_files_handles_empty_collection() {
        assert_eq!(completed_local_total_files(0), 0);
        assert_eq!(completed_local_total_files(1), 0);
        assert_eq!(completed_local_total_files(3), 2);
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
    fn receive_stream_ended_message_is_stable() {
        assert_eq!(
            receive_stream_ended_message(),
            "download stream ended before completion"
        );
    }

    #[test]
    fn process_get_stream_emits_failed_event_when_stream_ends_early() {
        let emitter = Arc::new(RecordingEmitter::default());
        let app_handle: crate::core::events::AppHandle = Some(emitter.clone());

        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let mut s = stream::empty::<GetProgressItem>();
            let err = process_get_stream(&mut s, 12, &app_handle)
                .await
                .expect_err("stream ending early should fail");
            assert!(err.to_string().contains("ended before completion"));
        });

        let events = emitter.events();
        assert!(matches!(
            events.first(),
            Some(TransferEvent::Progress {
                role: Role::Receiver,
                processed: 0,
                total: 12,
                ..
            })
        ));
        assert!(events.iter().any(|event| matches!(
            event,
            TransferEvent::Failed { role: Role::Receiver, message }
                if message == "download stream ended before completion"
        )));
    }

    #[test]
    fn emit_receive_failed_emits_receiver_failed_event() {
        let emitter = Arc::new(RecordingEmitter::default());
        let app_handle: crate::core::events::AppHandle = Some(emitter.clone());

        emit_receive_failed(&app_handle, "boom");

        let events = emitter.events();
        assert_eq!(events.len(), 1);
        match &events[0] {
            TransferEvent::Failed { role, message } => {
                assert_eq!(*role, Role::Receiver);
                assert_eq!(message, "boom");
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
        let err = process_get_stream(&mut s, 0, &None)
            .await
            .expect_err("stream ending early should fail");
        assert!(err.to_string().contains("ended before completion"));
    }
}
