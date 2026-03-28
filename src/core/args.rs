//! 命令行参数定义。
//!
//! 本文件定义：Args, Commands, SendArgs, ReceiveArgs, CommonArgs, Format。

use anyhow::Context;
use clap::{Parser, Subcommand};
use iroh_blobs::ticket::BlobTicket;
use std::fmt::{Display, Formatter};
use std::net::{SocketAddrV4, SocketAddrV6};
use std::path::PathBuf;
use std::str::FromStr;

use super::options::{AddrInfoOptions, RelayModeOption};

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
        |_| {
            let key = iroh::SecretKey::generate(&mut rand::rng());
            Ok(key)
        },
        |secret| iroh::SecretKey::from_str(&secret).context("invalid secret"),
    )
}