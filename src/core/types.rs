//! 核心类型与命令行参数定义。
//!
//! 本文件定义：事件发射器 trait、共享的配置结构、以及 CLI 参数解析相关的类型。

use anyhow::Context;
use clap::{Parser, Subcommand};
use iroh::{EndpointAddr, RelayUrl, TransportAddr};
use iroh_blobs::Hash;
use iroh_blobs::ticket::BlobTicket;
use std::fmt::{Display, Formatter};
use std::net::{SocketAddrV4, SocketAddrV6};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

/// 事件发射器接口。
///
/// 库代码通过该 trait 将 [`TransferEvent`]
/// 发送到 CLI / Tauri / GUI 等不同前端实现。
///
/// 设计约束：
/// - 不返回 `Result`
/// - 事件发送失败不得影响主流程
/// - 实现应尽量做到非阻塞
pub trait EventEmitter: Send + Sync {
    /// 发射一个传输事件。
    fn emit(&self, event: &TransferEvent);
}

/// 传输过程中对外发送的统一事件模型。
///
/// 该枚举用于描述一次传输在某个角色（发送端 / 接收端）下的
/// 生命周期状态变化。
///
/// ⚠️ 注意：
/// - 这是**通知型事件**，不参与错误控制流
/// - 不用于 `Result` / `anyhow`
/// - payload 直接体现在枚举字段中
#[derive(Debug, Clone)]
pub enum TransferEvent {
    /// 传输开始
    Started { role: Role },

    /// 传输进度更新
    Progress {
        role: Role,
        /// 已处理字节数
        processed: u64,
        /// 总字节数
        total: u64,
        /// 当前速度（字节 / 秒）
        speed: f64,
    },

    /// 传输完成
    Completed { role: Role },

    /// 传输失败
    Failed {
        role: Role,
        /// 用于展示的错误信息
        message: String,
    },

    /// 特殊事件：文件名列表
    /// 特殊事件：传递文件名列表
    FileNames { role: Role, file_names: Vec<String> },
}

impl TransferEvent {
    /// 返回事件状态字符串（started / progress / completed / failed）
    pub const fn state(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::Progress { .. } => "progress",
            Self::Completed { .. } => "completed",
            Self::Failed { .. } => "failed",
            Self::FileNames { .. } => "file-names",
        }
    }

    /// 返回事件所属角色
    pub const fn role(&self) -> Role {
        match self {
            Self::Started { role }
            | Self::Completed { role }
            | Self::Failed { role, .. }
            | Self::Progress { role, .. }
            | Self::FileNames { role, .. } => *role,
        }
    }

    /// 返回发送给 Tauri 前端的最终事件名
    ///
    /// 事件格式：
    /// `transfer:<role>:<state>`
    ///
    /// 示例：
    /// - `transfer:sender:started`
    /// - `transfer:receiver:progress`
    pub fn event_name(&self) -> String {
        format!("transfer:{}:{}", self.role().as_str(), self.state())
    }
}

/// 传输事件所属的角色（发送端 / 接收端）。
///
/// 用于区分事件来自哪一侧，
/// 前端与 CLI 可以据此展示不同视角的状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// 数据发送方
    Sender,
    /// 数据接收方
    Receiver,
}

impl Role {
    /// 用于事件字符串拼接（Tauri 前端）。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Sender => "sender",
            Self::Receiver => "receiver",
        }
    }
}

/// 应用层句柄：可选包装的共享 `EventEmitter`。
///
/// 使用 `None` 表示不发射任何事件（例如在测试或禁止进度时）。
pub type AppHandle = Option<Arc<dyn EventEmitter>>;

/// 安全地向前端发送事件。
///
/// 若未配置事件发射器或发送失败，将被忽略。
pub fn emit_event(app: &AppHandle, event: &TransferEvent) {
    if let Some(handle) = app {
        handle.emit(event);
    }
}

pub struct SendResult {
    pub ticket: String,
    pub hash: String,
    pub size: u64,
    pub entry_type: String, // "file" or "directory"

    // CRITICAL: These fields must be kept alive for the duration of the share
    pub router: iroh::protocol::Router, // Keeps the server running and protocols active
    pub temp_tag: iroh_blobs::api::TempTag, // Prevents data from being garbage collected
    pub blobs_data_dir: PathBuf,        // Path for cleanup when share stops
    pub _progress_handle: n0_future::task::AbortOnDropHandle<anyhow::Result<()>>, // Keeps event channel open
    pub _store: iroh_blobs::store::fs::FsStore, // Keeps the blob storage alive
}

// 以上结构都是内部核心类型，包含跨模块共享的返回值与资源句柄。

#[derive(Debug)]
pub struct ReceiveResult {
    pub message: String,
    pub file_path: PathBuf,
}

#[derive(Debug, Default)]
pub struct SendOptions {
    pub relay_mode: RelayModeOption,
    pub ticket_type: AddrInfoOptions,
    pub magic_ipv4_addr: Option<SocketAddrV4>,
    pub magic_ipv6_addr: Option<SocketAddrV6>,
}

#[derive(Debug, Default)]
pub struct ReceiveOptions {
    pub output_dir: Option<PathBuf>,
    pub relay_mode: RelayModeOption,
    pub magic_ipv4_addr: Option<SocketAddrV4>,
    pub magic_ipv6_addr: Option<SocketAddrV6>,
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

pub fn apply_options(addr: &mut EndpointAddr, opts: AddrInfoOptions) {
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

pub fn get_or_create_secret() -> anyhow::Result<iroh::SecretKey> {
    std::env::var("IROH_SECRET").map_or_else(
        |_| {
            let key = iroh::SecretKey::generate(&mut rand::rng());
            Ok(key)
        },
        |secret| iroh::SecretKey::from_str(&secret).context("invalid secret"),
    )
}

#[derive(Parser, Debug)]
#[command(version, about)]
pub struct Args {
    #[clap(subcommand)]
    pub command: Commands,
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

pub fn print_hash(hash: &Hash, format: Format) -> String {
    match format {
        Format::Hex => hash.to_hex(),
        Format::Cid => hash.to_string(),
    }
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Send a file or directory.
    Send(SendArgs),

    /// Receive a file or directory.
    #[clap(visible_alias = "recv")]
    Receive(ReceiveArgs),
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

    /// The relay URL to use as a home relay,
    ///
    /// Can be set to "disabled" to disable relay servers and "default"
    /// to configure default servers.
    #[clap(long, default_value_t = RelayModeOption::Default)]
    pub relay: RelayModeOption,

    #[clap(long)]
    pub show_secret: bool,
}

impl FromStr for RelayModeOption {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "disabled" => Ok(Self::Disabled),
            "default" => Ok(Self::Default),
            _ => Ok(Self::Custom(RelayUrl::from_str(s)?)),
        }
    }
}

impl Display for RelayModeOption {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("disabled"),
            Self::Default => f.write_str("default"),
            Self::Custom(url) => url.fmt(f),
        }
    }
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

    #[clap(flatten)]
    pub common: CommonArgs,
}
