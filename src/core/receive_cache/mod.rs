//! 持久接收缓存、跨进程锁和过期清理。
//!
//! 缓存模块只负责已验证传输数据的生命周期。它不读取 CLI 状态，也不控制网络连接。

#[cfg(not(feature = "persistent-recovery"))]
mod disabled;
#[cfg(feature = "persistent-recovery")]
mod lease;
#[cfg(feature = "persistent-recovery")]
mod model;
#[cfg(feature = "persistent-recovery")]
mod prune;
#[cfg(feature = "persistent-recovery")]
mod snapshot;

#[cfg(not(feature = "persistent-recovery"))]
pub use disabled::prune_receive_cache;
#[cfg(feature = "persistent-recovery")]
pub use prune::prune_receive_cache;

#[cfg(feature = "persistent-recovery")]
pub(super) use lease::ReceiveCacheLease;
/// Summary returned by receive-cache maintenance.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveCachePruneReport {
    pub removed_entries: u64,
    pub retained_entries: u64,
    pub active_entries: u64,
    pub unknown_entries: u64,
}

#[cfg(all(test, feature = "persistent-recovery"))]
use model::{
    CACHE_MANIFEST_FILE, CACHE_MIN_FREE_BYTES, CACHE_ROOT_LOCK_FILE, CACHE_SCHEMA_VERSION,
    ReceiveCacheManifest,
};
#[cfg(all(test, feature = "persistent-recovery"))]
use prune::{
    cache_directory_size, cache_key, ensure_cache_capacity_with_free_space, open_lock_file,
};

#[cfg(all(test, feature = "persistent-recovery"))]
mod tests;
