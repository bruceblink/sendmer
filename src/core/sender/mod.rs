//! 发送端功能：将本地文件/目录导入 Blob 存储并通过 iroh 协议对外提供。
//!
//! 主要导出 `start_share`，它会导入数据、启动路由器并返回用于后续管理的 `SendResult`。

use crate::core::endpoint::base_endpoint_builder;
use crate::core::events::{
    AppHandle, Role, TransferError, TransferErrorCode, TransferPhase, classified_transfer_error,
    classify_transfer_error, is_transfer_cancelled, transfer_cancelled_error,
};
use crate::core::manifest::{
    MANIFEST_COLLECTION_NAME, MANIFEST_ENTRY_PREFIX, ManifestEntry, ManifestEntryKind,
    ManifestMetadata, ManifestPath, ManifestPayload, ManifestPermissions, ManifestRoot,
    TransferManifest, UnixTimestamp,
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
    api::{Store, TempTag},
    format::collection::Collection,
    provider::events::{ConnectMode, EventMask, EventSender, RequestMode, ThrottleMode},
    store::fs::FsStore,
    ticket::BlobTicket,
};
use n0_future::StreamExt;
use n0_future::{BufferedStreamExt, task::AbortOnDropHandle};
use std::{
    collections::{BTreeSet, HashSet},
    future::Future,
    num::NonZeroU64,
    path::{Component, Path, PathBuf},
    pin::Pin,
    time::Duration,
};
use tokio::{
    select,
    sync::{Notify, Semaphore, mpsc, watch},
    task::JoinSet,
    time::Instant,
};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};
use walkdir::WalkDir;

const PROVIDER_PROGRESS_TASK_LIMIT: usize = 32;
const ENDPOINT_ONLINE_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

include!("validation.rs");
include!("setup.rs");
include!("provider_setup.rs");
include!("lifecycle.rs");
include!("import.rs");
include!("provider.rs");

#[cfg(test)]
mod tests {
    include!("tests_a.rs");
    include!("tests_b.rs");
}
