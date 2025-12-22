use iroh::{EndpointAddr, TransportAddr};
use crate::types::AddrInfoOptions;

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