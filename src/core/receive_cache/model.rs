use serde::{Deserialize, Serialize};

pub const CACHE_LAYOUT_DIR: &str = "v1";
pub const CACHE_SCHEMA_VERSION: u32 = 1;
pub const CACHE_MANIFEST_FILE: &str = "manifest.json";
pub const CACHE_LOCK_FILE: &str = ".lock";
pub const CACHE_ROOT_LOCK_FILE: &str = ".prune.lock";
pub const MANIFEST_TEMP_PREFIX: &str = ".manifest-";
/// 缓存目录的总字节硬上限，防止显式缓存配置持续占满磁盘。
pub const CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024 * 1024;
/// 打开缓存前必须保留的文件系统可用空间。
pub const CACHE_MIN_FREE_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Deserialize, Serialize)]
pub struct ReceiveCacheManifest {
    pub schema_version: u32,
    pub cache_key: String,
    pub created_at_unix_seconds: u64,
    pub ttl_seconds: u64,
}
