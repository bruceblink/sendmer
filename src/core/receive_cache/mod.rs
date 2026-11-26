//! 持久接收缓存、跨进程锁和过期清理。
//!
//! 缓存模块只负责已验证传输数据的生命周期。它不读取 CLI 状态，也不控制网络连接。

mod lease;
mod model;
mod prune;
mod snapshot;

pub use model::ReceiveCachePruneReport;
pub use prune::prune_receive_cache;

pub(super) use lease::ReceiveCacheLease;
#[cfg(test)]
use model::{
    CACHE_MANIFEST_FILE, CACHE_MIN_FREE_BYTES, CACHE_ROOT_LOCK_FILE, CACHE_SCHEMA_VERSION,
    ReceiveCacheManifest,
};
#[cfg(test)]
use prune::{
    cache_directory_size, cache_key, ensure_cache_capacity_with_free_space, open_lock_file,
};

#[cfg(test)]
mod tests;
