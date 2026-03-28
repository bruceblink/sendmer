//! 发送和接收结果定义。
//!
//! 本文件定义：SendResult, ReceiveResult。

use iroh_blobs::{Hash, ticket::BlobTicket};
use std::path::PathBuf;
use crate::core::types::EntryType;

/// 发送结果结构体。
pub struct SendResult {
    pub ticket: BlobTicket,
    pub hash: Hash,
    pub size: u64,
    pub entry_type: EntryType,

    // CRITICAL: These fields must be kept alive for the duration of the share
    pub router: iroh::protocol::Router, // Keeps the server running and protocols active
    pub temp_tag: iroh_blobs::api::TempTag, // Prevents data from being garbage collected
    pub blobs_data_dir: PathBuf,        // Path for cleanup when share stops
    pub _progress_handle: n0_future::task::AbortOnDropHandle<anyhow::Result<()>>, // Keeps event channel open
    pub _store: iroh_blobs::store::fs::FsStore, // Keeps the blob storage alive
}

/// 接收结果结构体。
#[derive(Debug)]
pub struct ReceiveResult {
    pub message: String,
    pub file_path: PathBuf,
}