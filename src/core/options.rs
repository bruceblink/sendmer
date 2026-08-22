//! 发送和接收选项定义。
//!
//! 本文件定义：SendOptions, ReceiveOptions, RelayModeOption, AddrInfoOptions。

use iroh::RelayUrl;
use std::net::{SocketAddr, SocketAddrV4, SocketAddrV6};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct SendOptions {
    pub relay_mode: RelayModeOption,
    pub ticket_type: AddrInfoOptions,
    pub magic_ipv4_addr: Option<SocketAddrV4>,
    pub magic_ipv6_addr: Option<SocketAddrV6>,
    /// Optional shared payload upload ceiling for every receiver of this share.
    ///
    /// `None` preserves the unrestricted provider behavior. A non-zero rate is
    /// enforced by the sender after each payload chunk, rather than by changing
    /// QUIC transport windows or local file-import behavior.
    pub max_upload_rate_bytes_per_sec: Option<NonZeroU64>,
    /// Optional ceiling for simultaneously connected receiver peers.
    ///
    /// `None` preserves the unlimited sender behavior. A disconnected receiver
    /// releases its slot for a later connection.
    pub max_receivers: Option<NonZeroU64>,
    /// Optional ceiling for regular files included in a shared directory.
    ///
    /// `None` preserves the unlimited sender behavior. The limit is checked
    /// before networking or temporary sender storage is initialized.
    pub max_files: Option<NonZeroU64>,
    /// Optional ceiling for the total bytes in regular files included in a share.
    ///
    /// `None` preserves the unlimited sender behavior. The limit is checked
    /// from file metadata before networking or temporary sender storage is initialized.
    pub max_total_size_bytes: Option<NonZeroU64>,
    /// Optional estimated file-payload budget for concurrent sender imports.
    ///
    /// `None` preserves the existing CPU-based import concurrency. When set, the
    /// sender schedules concurrent imports so their source-file bytes in flight do
    /// not exceed this budget; it is not a process RSS or operating-system limit.
    pub max_import_memory_bytes: Option<NonZeroU64>,
}

#[derive(Debug, Clone, Copy)]
pub struct ReceiveRetryPolicy {
    pub size_fetch_retry_limit: u32,
    pub size_fetch_chunk_size: u64,
    pub size_fetch_backoff_ms: u64,
    pub download_retry_limit: u32,
    pub download_retry_backoff_ms: u64,
    pub connect_timeout_ms: Option<u64>,
    pub metadata_timeout_ms: Option<u64>,
    pub download_idle_timeout_ms: Option<u64>,
}

impl Default for ReceiveRetryPolicy {
    fn default() -> Self {
        Self {
            size_fetch_retry_limit: 3,
            size_fetch_chunk_size: 1024 * 1024 * 32,
            size_fetch_backoff_ms: 250,
            download_retry_limit: 3,
            download_retry_backoff_ms: 250,
            connect_timeout_ms: None,
            metadata_timeout_ms: None,
            download_idle_timeout_ms: None,
        }
    }
}

impl ReceiveRetryPolicy {
    /// Validate receive retry settings before network and temporary-store setup begins.
    pub fn validate(self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.size_fetch_retry_limit > 0,
            "size-fetch retry limit must be greater than zero"
        );
        anyhow::ensure!(
            self.size_fetch_chunk_size > 0,
            "size-fetch chunk size must be greater than zero"
        );
        anyhow::ensure!(
            self.download_retry_limit > 0,
            "download retry limit must be greater than zero"
        );
        for (name, timeout_ms) in [
            ("connect timeout", self.connect_timeout_ms),
            ("metadata timeout", self.metadata_timeout_ms),
            ("download idle timeout", self.download_idle_timeout_ms),
        ] {
            anyhow::ensure!(
                timeout_ms != Some(0),
                "{name} must be greater than zero when configured"
            );
        }
        Ok(())
    }

    /// Convert configured receive limits into durations after validation.
    pub(crate) fn connect_timeout(self) -> Option<Duration> {
        self.connect_timeout_ms.map(Duration::from_millis)
    }

    /// Limit metadata requests without imposing a deadline on file payloads.
    pub(crate) fn metadata_timeout(self) -> Option<Duration> {
        self.metadata_timeout_ms.map(Duration::from_millis)
    }

    /// Reset the download watchdog whenever the remote stream emits an item.
    pub(crate) fn download_idle_timeout(self) -> Option<Duration> {
        self.download_idle_timeout_ms.map(Duration::from_millis)
    }
}

/// Opt-in persistent storage for receive-side verified blob data.
///
/// A cache entry is keyed by content hash and blob format. Failed or cancelled
/// receives keep the entry for a later process, while a successful export
/// removes it to avoid retaining a second copy of the user's data.
#[derive(Debug, Clone)]
pub struct ReceiveCacheOptions {
    pub root_dir: PathBuf,
    pub ttl: Duration,
}

impl ReceiveCacheOptions {
    pub const DEFAULT_TTL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    /// Create persistent receive-cache options with the default seven-day TTL.
    pub fn new(root_dir: impl Into<PathBuf>) -> Self {
        Self {
            root_dir: root_dir.into(),
            ttl: Self::DEFAULT_TTL,
        }
    }

    /// Replace the default TTL used by later cache-pruning operations.
    pub const fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    /// Reject unsafe or meaningless cache settings before network setup begins.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.root_dir.as_os_str().is_empty(),
            "receive cache root must not be empty"
        );
        anyhow::ensure!(
            !self.ttl.is_zero(),
            "receive cache TTL must be greater than zero"
        );

        match std::fs::symlink_metadata(&self.root_dir) {
            Ok(metadata) => {
                anyhow::ensure!(
                    !metadata.file_type().is_symlink(),
                    "receive cache root must not be a symbolic link"
                );
                anyhow::ensure!(metadata.is_dir(), "receive cache root must be a directory");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct ReceiveOptions {
    pub output_dir: Option<std::path::PathBuf>,
    pub relay_mode: RelayModeOption,
    pub magic_ipv4_addr: Option<SocketAddrV4>,
    pub magic_ipv6_addr: Option<SocketAddrV6>,
    pub retry_policy: ReceiveRetryPolicy,
    pub receive_cache: Option<ReceiveCacheOptions>,
}

pub trait EndpointOptions: BindAddressOptions {
    fn relay_mode(&self) -> RelayModeOption;
}

pub trait BindAddressOptions {
    fn magic_ipv4_addr(&self) -> Option<SocketAddrV4>;
    fn magic_ipv6_addr(&self) -> Option<SocketAddrV6>;
}

impl EndpointOptions for SendOptions {
    fn relay_mode(&self) -> RelayModeOption {
        self.relay_mode.clone()
    }
}

impl BindAddressOptions for SendOptions {
    fn magic_ipv4_addr(&self) -> Option<SocketAddrV4> {
        self.magic_ipv4_addr
    }

    fn magic_ipv6_addr(&self) -> Option<SocketAddrV6> {
        self.magic_ipv6_addr
    }
}

impl EndpointOptions for ReceiveOptions {
    fn relay_mode(&self) -> RelayModeOption {
        self.relay_mode.clone()
    }
}

impl BindAddressOptions for ReceiveOptions {
    fn magic_ipv4_addr(&self) -> Option<SocketAddrV4> {
        self.magic_ipv4_addr
    }

    fn magic_ipv6_addr(&self) -> Option<SocketAddrV6> {
        self.magic_ipv6_addr
    }
}

pub fn apply_bind_addrs<T: BindAddressOptions>(
    mut builder: iroh::endpoint::Builder,
    options: &T,
) -> anyhow::Result<iroh::endpoint::Builder> {
    if let Some(addr) = options.magic_ipv4_addr() {
        builder = builder.bind_addr(SocketAddr::V4(addr))?;
    }
    if let Some(addr) = options.magic_ipv6_addr() {
        builder = builder.bind_addr(SocketAddr::V6(addr))?;
    }
    Ok(builder)
}

#[derive(Clone, Debug, Default)]
pub enum RelayModeOption {
    Disabled,
    #[default]
    Default,
    Custom(RelayUrl),
}

impl From<RelayModeOption> for iroh::RelayMode {
    fn from(value: RelayModeOption) -> Self {
        match value {
            RelayModeOption::Disabled => Self::Disabled,
            RelayModeOption::Default => Self::Default,
            RelayModeOption::Custom(url) => Self::Custom(url.into()),
        }
    }
}

impl std::str::FromStr for RelayModeOption {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "disabled" => Ok(Self::Disabled),
            "default" => Ok(Self::Default),
            _ => Ok(Self::Custom(RelayUrl::from_str(s)?)),
        }
    }
}

impl std::fmt::Display for RelayModeOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("disabled"),
            Self::Default => f.write_str("default"),
            Self::Custom(url) => url.fmt(f),
        }
    }
}

#[derive(
    Copy,
    Clone,
    PartialEq,
    Eq,
    Default,
    Debug,
    derive_more::Display,
    derive_more::FromStr,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum AddrInfoOptions {
    #[default]
    Id,
    RelayAndAddresses,
    Relay,
    Addresses,
}
pub fn apply_options(addr: &mut iroh::EndpointAddr, opts: AddrInfoOptions) {
    use iroh::TransportAddr;
    match opts {
        AddrInfoOptions::Id => {
            addr.addrs = Default::default();
        }
        AddrInfoOptions::RelayAndAddresses => {
            // nothing to do
        }
        AddrInfoOptions::Relay => {
            addr.addrs = addr
                .addrs
                .iter()
                .filter(|addr| matches!(addr, TransportAddr::Relay(_)))
                .cloned()
                .collect();
        }
        AddrInfoOptions::Addresses => {
            addr.addrs = addr
                .addrs
                .iter()
                .filter(|addr| matches!(addr, TransportAddr::Ip(_)))
                .cloned()
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ReceiveCacheOptions, ReceiveRetryPolicy, SendOptions};
    use std::time::Duration;

    #[test]
    fn receive_retry_policy_defaults_match_receiver_expectations() {
        let policy = ReceiveRetryPolicy::default();
        assert_eq!(policy.size_fetch_retry_limit, 3);
        assert_eq!(policy.size_fetch_chunk_size, 1024 * 1024 * 32);
        assert_eq!(policy.size_fetch_backoff_ms, 250);
        assert_eq!(policy.download_retry_limit, 3);
        assert_eq!(policy.download_retry_backoff_ms, 250);
        assert_eq!(policy.connect_timeout_ms, None);
        assert_eq!(policy.metadata_timeout_ms, None);
        assert_eq!(policy.download_idle_timeout_ms, None);
    }

    #[test]
    fn send_options_default_to_unlimited_resource_limits() {
        let options = SendOptions::default();
        assert!(options.max_receivers.is_none());
        assert!(options.max_files.is_none());
        assert!(options.max_total_size_bytes.is_none());
        assert!(options.max_import_memory_bytes.is_none());
    }

    #[test]
    fn receive_retry_policy_rejects_zero_retry_limit() {
        let policy = ReceiveRetryPolicy {
            size_fetch_retry_limit: 0,
            ..Default::default()
        };

        let error = policy
            .validate()
            .expect_err("zero retries should be rejected");
        assert!(error.to_string().contains("retry limit"));
    }

    #[test]
    fn receive_retry_policy_rejects_zero_chunk_size() {
        let policy = ReceiveRetryPolicy {
            size_fetch_chunk_size: 0,
            ..Default::default()
        };

        let error = policy
            .validate()
            .expect_err("zero chunk size should be rejected");
        assert!(error.to_string().contains("chunk size"));
    }

    #[test]
    fn receive_retry_policy_rejects_zero_download_retry_limit() {
        let policy = ReceiveRetryPolicy {
            download_retry_limit: 0,
            ..Default::default()
        };

        let error = policy
            .validate()
            .expect_err("zero download retries should be rejected");
        assert!(error.to_string().contains("download retry limit"));
    }

    #[test]
    fn receive_retry_policy_allows_zero_backoff() {
        let policy = ReceiveRetryPolicy {
            size_fetch_backoff_ms: 0,
            ..Default::default()
        };

        policy
            .validate()
            .expect("zero backoff should allow immediate retries");
    }

    #[test]
    fn receive_retry_policy_rejects_zero_configured_timeouts() {
        for policy in [
            ReceiveRetryPolicy {
                connect_timeout_ms: Some(0),
                ..Default::default()
            },
            ReceiveRetryPolicy {
                metadata_timeout_ms: Some(0),
                ..Default::default()
            },
            ReceiveRetryPolicy {
                download_idle_timeout_ms: Some(0),
                ..Default::default()
            },
        ] {
            let error = policy
                .validate()
                .expect_err("zero timeout should be rejected");
            assert!(error.to_string().contains("timeout"));
        }
    }

    #[test]
    fn receive_cache_options_default_to_seven_days() {
        let options = ReceiveCacheOptions::new("cache");
        assert_eq!(options.ttl, Duration::from_secs(7 * 24 * 60 * 60));
        options.validate().expect("default cache options");
    }

    #[test]
    fn receive_cache_options_reject_zero_ttl() {
        let options = ReceiveCacheOptions::new("cache").with_ttl(Duration::ZERO);
        let error = options.validate().expect_err("zero TTL must fail");
        assert!(error.to_string().contains("TTL"));
    }

    #[test]
    fn receive_cache_options_reject_file_root() {
        let temp = tempfile::tempdir().expect("temp dir");
        let file = temp.path().join("cache-file");
        std::fs::write(&file, b"not a directory").expect("cache file");

        let error = ReceiveCacheOptions::new(file)
            .validate()
            .expect_err("file cache root must fail");
        assert!(error.to_string().contains("directory"));
    }
}
