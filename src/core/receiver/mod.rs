//! 接收端功能：根据票据连接远端并导出数据到本地目录。
//!
//! 主要导出 `download`，它负责建立连接、跟踪进度并将文件导出到目标目录。

use crate::core::endpoint::base_endpoint_builder;
use crate::core::events::{
    AppHandle, Role, TransferError, TransferErrorCode, TransferPhase, classified_transfer_error,
    classify_transfer_error, is_transfer_cancelled, transfer_cancelled_error,
};
use crate::core::filesystem::*;
use crate::core::manifest::{MANIFEST_COLLECTION_NAME, ManifestEntryKind, TransferManifest};
use crate::core::options::{
    MAX_LEGACY_COLLECTION_ITEMS, MAX_LEGACY_COLLECTION_METADATA_BYTES, ReceiveLimits,
    ReceiveOptions, ReceiveRetryPolicy,
};
use crate::core::progress::{ReceiverProgressReporter, TransferEventEmitter};
use crate::core::receive_cache::ReceiveCacheLease;
use crate::core::results::ReceiveResult;
use crate::core::shutdown::with_shutdown_deadline;
use crate::core::storage::{load_fs_store, unique_temp_dir};
use anyhow::Context;
use iroh::{Endpoint, address_lookup::DnsAddressLookup};
use iroh_blobs::{
    api::{Store, proto::BlobStatus, remote::GetProgressItem},
    format::collection::Collection,
    get::{GetError, request::get_hash_seq_and_sizes},
    ticket::BlobTicket,
};
use n0_future::StreamExt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc as StdArc;
use std::time::Duration;
use tokio::select;
use tracing::info;

// event helpers provided by `core::progress`

pub(crate) const RECEIVE_TEMP_DIR_PREFIX: &str = ".sendmer-recv-";

include!("entry.rs");
include!("context.rs");
include!("download.rs");
include!("retry.rs");
include!("prepare.rs");

#[cfg(test)]
mod tests {
    include!("tests_a.rs");
    include!("tests_b.rs");
    include!("tests_c.rs");
}
