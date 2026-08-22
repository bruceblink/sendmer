//! 命令行参数定义。
//!
//! 本文件定义：Args, Commands, SendArgs, ReceiveArgs, CommonArgs, Format。

use anyhow::Context;
use clap::{Parser, Subcommand};
use iroh_blobs::ticket::BlobTicket;
use std::fmt::{Display, Formatter};
use std::net::{SocketAddrV4, SocketAddrV6};
use std::num::NonZeroU64;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::OnceLock;

use super::options::{AddrInfoOptions, RelayModeOption};

static PROCESS_SECRET: OnceLock<iroh::SecretKey> = OnceLock::new();

#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
    #[clap(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Send a file or directory.
    Send(SendArgs),
    /// Receive a file or directory.
    #[clap(visible_alias = "recv")]
    Receive(ReceiveArgs),
    /// Inspect and maintain persistent receive-cache data.
    Cache(CacheArgs),
}

#[derive(Parser, Debug)]
pub struct CacheArgs {
    #[clap(subcommand)]
    pub command: CacheCommands,
}

#[derive(Subcommand, Debug)]
pub enum CacheCommands {
    /// Remove expired entries while preserving active or unknown data.
    Prune(CachePruneArgs),
}

#[derive(Parser, Debug)]
pub struct CachePruneArgs {
    /// Persistent receive-cache root to inspect.
    #[clap(long)]
    pub cache_dir: PathBuf,
}

#[derive(Parser, Debug)]
pub struct CommonArgs {
    /// The IPv4 address that magicsocket will listen on.
    ///
    /// If None, defaults to a random free port, but it can be useful to specify a fixed
    /// port, e.g. to configure a firewall rule.
    #[clap(long, default_value = None)]
    pub magic_ipv4_addr: Option<SocketAddrV4>,

    /// The IPv6 address that magicsocket will listen on.
    ///
    /// If None, defaults to a random free port, but it can be useful to specify a fixed
    /// port, e.g. to configure a firewall rule.
    #[clap(long, default_value = None)]
    pub magic_ipv6_addr: Option<SocketAddrV6>,

    #[clap(long, default_value_t = Format::Hex)]
    pub format: Format,

    #[clap(short = 'v', long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Suppress progress bars.
    #[clap(long, default_value_t = false)]
    pub no_progress: bool,

    /// Emit transfer events as JSON Lines on stdout instead of progress bars.
    #[clap(long, default_value_t = false)]
    pub json_events: bool,

    /// The relay URL to use as a home relay,
    ///
    /// Can be set to "disabled" to disable relay servers and "default"
    /// to configure default servers.
    #[clap(long, default_value_t = RelayModeOption::Default)]
    pub relay: RelayModeOption,

    #[clap(long)]
    pub show_secret: bool,
}

#[derive(Parser, Debug)]
pub struct SendArgs {
    /// Path to the file or directory to send.
    ///
    /// The last component of the path will be used as the name of the data
    /// being shared.
    pub path: PathBuf,

    /// What type of ticket to use.
    ///
    /// Use "id" for the shortest type only including the node ID,
    /// "addresses" to only add IP addresses without a relay url,
    /// "relay" to only add a relay address, and leave the option out
    /// to use the biggest type of ticket that includes both relay and
    /// address information.
    ///
    /// Generally, the more information the higher the likelyhood of
    /// a successful connection, but also the bigger a ticket to connect.
    ///
    /// This is most useful for debugging which methods of connection
    /// establishment work well.
    #[clap(long, default_value_t = AddrInfoOptions::RelayAndAddresses)]
    pub ticket_type: AddrInfoOptions,

    /// Optional shared payload upload ceiling in bytes per second.
    ///
    /// The limit is shared by all receivers for this send command. It does not
    /// include relay, QUIC, or other protocol overhead.
    #[clap(long)]
    pub max_upload_rate: Option<NonZeroU64>,

    /// Maximum number of receiver connections that may be active at once.
    #[clap(long)]
    pub max_receivers: Option<NonZeroU64>,

    /// Maximum number of regular files included in the shared path.
    #[clap(long)]
    pub max_files: Option<NonZeroU64>,

    /// Maximum total size in bytes of regular files included in the shared path.
    #[clap(long)]
    pub max_total_size: Option<NonZeroU64>,

    #[clap(flatten)]
    pub common: CommonArgs,

    /// Store the receive command in the clipboard.
    #[cfg(feature = "clipboard")]
    #[clap(short = 'c', long)]
    pub clipboard: bool,
}

#[derive(Parser, Debug)]
pub struct ReceiveArgs {
    /// The ticket to use to connect to the sender.
    pub ticket: BlobTicket,

    /// Output directory for received files.
    ///
    /// Defaults to the current working directory when omitted.
    #[clap(long)]
    pub output_dir: Option<PathBuf>,

    /// Maximum number of attempts for the blob download phase.
    #[clap(long, default_value_t = 3)]
    pub retry_limit: u32,

    /// Delay in milliseconds between blob download retries.
    #[clap(long, default_value_t = 250)]
    pub retry_backoff_ms: u64,

    /// Optional connection timeout in milliseconds for each receive attempt.
    #[clap(long)]
    pub connect_timeout_ms: Option<u64>,

    /// Optional timeout in milliseconds while fetching collection metadata.
    #[clap(long)]
    pub metadata_timeout_ms: Option<u64>,

    /// Optional timeout in milliseconds without download-stream progress.
    #[clap(long)]
    pub download_idle_timeout_ms: Option<u64>,

    /// Optional directory for receive data that can resume in a later process.
    #[clap(long)]
    pub cache_dir: Option<PathBuf>,

    /// Lifetime in seconds recorded for a persistent receive-cache entry.
    #[clap(long, default_value_t = 7 * 24 * 60 * 60)]
    pub cache_ttl_seconds: u64,

    #[clap(flatten)]
    pub common: CommonArgs,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    #[default]
    Hex,
    Cid,
}

impl FromStr for Format {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "hex" => Ok(Self::Hex),
            "cid" => Ok(Self::Cid),
            _ => Err(anyhow::anyhow!("invalid format")),
        }
    }
}

impl Display for Format {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Hex => write!(f, "hex"),
            Self::Cid => write!(f, "cid"),
        }
    }
}

pub fn print_hash(hash: &iroh_blobs::Hash, format: Format) -> String {
    match format {
        Format::Hex => hash.to_hex(),
        Format::Cid => hash.to_string(),
    }
}

pub fn get_or_create_secret() -> anyhow::Result<iroh::SecretKey> {
    std::env::var("IROH_SECRET").map_or_else(
        |_| Ok(PROCESS_SECRET.get_or_init(new_secret_key).clone()),
        |secret| iroh::SecretKey::from_str(&secret).context("invalid secret"),
    )
}

fn new_secret_key() -> iroh::SecretKey {
    iroh::SecretKey::generate()
}

#[cfg(test)]
mod tests {
    use super::{Args, Commands};
    use clap::Parser;
    use iroh::EndpointAddr;
    use iroh_blobs::{BlobFormat, Hash, ticket::BlobTicket};
    use std::num::NonZeroU64;
    use std::path::PathBuf;

    fn sample_ticket() -> String {
        BlobTicket::new(
            EndpointAddr::new(iroh::SecretKey::generate().public()),
            Hash::new(b"receive argument test"),
            BlobFormat::HashSeq,
        )
        .to_string()
    }

    #[test]
    fn receive_args_accept_explicit_download_retry_values() {
        let ticket = sample_ticket();
        let args = Args::try_parse_from([
            "sendmer",
            "receive",
            "--retry-limit",
            "7",
            "--retry-backoff-ms",
            "125",
            "--connect-timeout-ms",
            "1000",
            "--metadata-timeout-ms",
            "2000",
            "--download-idle-timeout-ms",
            "3000",
            "--cache-dir",
            "receive-cache",
            "--cache-ttl-seconds",
            "86400",
            ticket.as_str(),
        ])
        .expect("valid receive arguments");

        let Commands::Receive(receive) = args.command else {
            panic!("expected receive command")
        };
        assert_eq!(receive.retry_limit, 7);
        assert_eq!(receive.retry_backoff_ms, 125);
        assert_eq!(receive.connect_timeout_ms, Some(1000));
        assert_eq!(receive.metadata_timeout_ms, Some(2000));
        assert_eq!(receive.download_idle_timeout_ms, Some(3000));
        assert_eq!(receive.cache_dir, Some(PathBuf::from("receive-cache")));
        assert_eq!(receive.cache_ttl_seconds, 86_400);
    }

    #[test]
    fn send_args_accept_non_zero_upload_rate() {
        let args = Args::try_parse_from([
            "sendmer",
            "send",
            "--max-upload-rate",
            "1048576",
            "example.bin",
        ])
        .expect("valid send arguments");

        let Commands::Send(send) = args.command else {
            panic!("expected send command")
        };
        assert_eq!(send.max_upload_rate.map(NonZeroU64::get), Some(1_048_576));
    }

    #[test]
    fn send_args_reject_zero_upload_rate() {
        let error =
            Args::try_parse_from(["sendmer", "send", "--max-upload-rate", "0", "example.bin"])
                .expect_err("zero upload rate must be rejected");

        assert!(error.to_string().contains("invalid value"));
    }

    #[test]
    fn send_args_accept_non_zero_receiver_limit() {
        let args = Args::try_parse_from(["sendmer", "send", "--max-receivers", "2", "example.bin"])
            .expect("valid receiver limit");

        let Commands::Send(send) = args.command else {
            panic!("expected send command")
        };
        assert_eq!(send.max_receivers.map(NonZeroU64::get), Some(2));
    }

    #[test]
    fn send_args_reject_zero_receiver_limit() {
        let error =
            Args::try_parse_from(["sendmer", "send", "--max-receivers", "0", "example.bin"])
                .expect_err("zero receiver limit must be rejected");

        assert!(error.to_string().contains("invalid value"));
    }

    #[test]
    fn send_args_accept_non_zero_file_limit() {
        let args = Args::try_parse_from(["sendmer", "send", "--max-files", "10", "example.bin"])
            .expect("valid file limit");

        let Commands::Send(send) = args.command else {
            panic!("expected send command")
        };
        assert_eq!(send.max_files.map(NonZeroU64::get), Some(10));
    }

    #[test]
    fn send_args_reject_zero_file_limit() {
        let error = Args::try_parse_from(["sendmer", "send", "--max-files", "0", "example.bin"])
            .expect_err("zero file limit must be rejected");

        assert!(error.to_string().contains("invalid value"));
    }

    #[test]
    fn send_args_accept_non_zero_total_size_limit() {
        let args =
            Args::try_parse_from(["sendmer", "send", "--max-total-size", "4096", "example.bin"])
                .expect("valid total size limit");

        let Commands::Send(send) = args.command else {
            panic!("expected send command")
        };
        assert_eq!(send.max_total_size.map(NonZeroU64::get), Some(4_096));
    }

    #[test]
    fn send_args_reject_zero_total_size_limit() {
        let error =
            Args::try_parse_from(["sendmer", "send", "--max-total-size", "0", "example.bin"])
                .expect_err("zero total size limit must be rejected");

        assert!(error.to_string().contains("invalid value"));
    }

    #[test]
    fn common_args_accept_json_events() {
        let args = Args::try_parse_from(["sendmer", "send", "--json-events", "example.bin"])
            .expect("json event output should be accepted");

        let Commands::Send(send) = args.command else {
            panic!("expected send command")
        };
        assert!(send.common.json_events);
    }

    #[test]
    fn cache_prune_args_require_explicit_root() {
        let args =
            Args::try_parse_from(["sendmer", "cache", "prune", "--cache-dir", "receive-cache"])
                .expect("valid cache prune arguments");

        let Commands::Cache(cache) = args.command else {
            panic!("expected cache command")
        };
        let super::CacheCommands::Prune(prune) = cache.command;
        assert_eq!(prune.cache_dir, PathBuf::from("receive-cache"));
    }
}
