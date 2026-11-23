//! Versioned transfer manifest primitives for v0.10 directory semantics.
//!
//! The legacy iroh Collection stores UTF-8 names only. TM1 keeps the logical
//! tree and platform metadata in a separately validated JSON payload so the
//! receiver can reject unsafe or unsupported entries before touching staging.

use serde::{Deserialize, Serialize};
/// The highest transfer-manifest schema understood by this crate.
pub const MANIFEST_SCHEMA_VERSION: u16 = 1;

/// Reserved iroh Collection name for the raw TM1 JSON blob.
pub const MANIFEST_COLLECTION_NAME: &str = ".sendmer/manifest-v1";

/// Prefix for deterministic payload names in a manifest-mode collection.
pub const MANIFEST_ENTRY_PREFIX: &str = ".sendmer/entry/";

/// 接收端允许解析的 manifest JSON 最大字节数。
pub const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;
/// 接收端允许解析的 manifest 条目数量上限。
pub const MAX_MANIFEST_ENTRIES: usize = 100_000;
/// 单个路径允许的最大组件数量。
pub const MAX_MANIFEST_PATH_DEPTH: usize = 256;
/// 单个路径组件在传输编码后的最大字节数。
pub const MAX_MANIFEST_COMPONENT_BYTES: usize = 4096;
/// 路径组件和分隔符的最大总字节数。
pub const MAX_MANIFEST_PATH_BYTES: usize = 32 * 1024;

/// A versioned logical tree carried alongside payload blobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferManifest {
    pub schema_version: u16,
    pub root: ManifestRoot,
    pub entries: Vec<ManifestEntry>,
}

/// The one user-visible root represented by a transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRoot {
    pub kind: ManifestEntryKind,
    pub path: ManifestPath,
}

/// A logical file or directory in the transfer tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    pub kind: ManifestEntryKind,
    pub path: ManifestPath,
    pub payload: Option<ManifestPayload>,
    pub metadata: ManifestMetadata,
}

/// Whether a manifest entry is a regular file or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestEntryKind {
    File,
    Directory,
}

/// The collection mapping and declared byte length for one file payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPayload {
    pub collection_name: String,
    pub size: u64,
}

/// Metadata that can be applied after all payloads have been verified.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestMetadata {
    pub permissions: Option<ManifestPermissions>,
    pub modified: Option<UnixTimestamp>,
}

/// Portable subset of platform permissions supported by TM1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPermissions {
    pub posix_mode: Option<u32>,
    pub windows_read_only: Option<bool>,
}

/// Signed Unix timestamp with an explicit nanosecond remainder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnixTimestamp {
    pub seconds: i64,
    pub nanos: u32,
}

/// A path whose components are losslessly represented on the sending platform.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ManifestPath {
    pub encoding: PathEncoding,
    pub components: Vec<String>,
}

/// Wire encoding used for every component in one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathEncoding {
    Utf8,
    UnixBytesHex,
    WindowsUtf16LeHex,
}
