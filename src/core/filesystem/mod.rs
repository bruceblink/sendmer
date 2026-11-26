//! 文件系统 staging、路径检查和原子提交。
//!
//! 本模块只负责本地目标路径和完整导出提交，不控制网络连接或恢复缓存。

use crate::core::events::{
    TransferError, TransferErrorCode, TransferPhase, classify_transfer_error,
};
use crate::core::manifest::validate_windows_utf8_component;
#[cfg(feature = "manifest")]
use crate::core::manifest::{ManifestEntryKind, TransferManifest};
use iroh_blobs::{
    api::{
        Store,
        blobs::{ExportMode, ExportOptions, ExportProgressItem},
    },
    format::collection::Collection,
};
use n0_future::StreamExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

include!("export.rs");
include!("paths.rs");
