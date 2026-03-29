//! 发送和接收选项定义。
//!
//! 本文件定义：SendOptions, ReceiveOptions, RelayModeOption, AddrInfoOptions。

use iroh::RelayUrl;
use std::net::{SocketAddrV4, SocketAddrV6};

#[derive(Debug, Default)]
pub struct SendOptions {
    pub relay_mode: RelayModeOption,
    pub ticket_type: AddrInfoOptions,
    pub magic_ipv4_addr: Option<SocketAddrV4>,
    pub magic_ipv6_addr: Option<SocketAddrV6>,
}

#[derive(Debug, Default)]
pub struct ReceiveOptions {
    pub output_dir: Option<std::path::PathBuf>,
    pub relay_mode: RelayModeOption,
    pub magic_ipv4_addr: Option<SocketAddrV4>,
    pub magic_ipv6_addr: Option<SocketAddrV6>,
}

pub trait BindAddressOptions {
    fn magic_ipv4_addr(&self) -> Option<SocketAddrV4>;
    fn magic_ipv6_addr(&self) -> Option<SocketAddrV6>;
}

impl BindAddressOptions for SendOptions {
    fn magic_ipv4_addr(&self) -> Option<SocketAddrV4> {
        self.magic_ipv4_addr
    }

    fn magic_ipv6_addr(&self) -> Option<SocketAddrV6> {
        self.magic_ipv6_addr
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
) -> iroh::endpoint::Builder {
    if let Some(addr) = options.magic_ipv4_addr() {
        builder = builder.bind_addr_v4(addr);
    }
    if let Some(addr) = options.magic_ipv6_addr() {
        builder = builder.bind_addr_v6(addr);
    }
    builder
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
