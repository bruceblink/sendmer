//! Shared endpoint builder helpers for sender and receiver flows.

use crate::core::args::get_or_create_secret;
use crate::core::options::EndpointOptions;
use iroh::endpoint::presets;

pub fn base_endpoint_builder<T: EndpointOptions>(
    options: &T,
    alpns: Vec<Vec<u8>>,
) -> anyhow::Result<iroh::endpoint::Builder> {
    let secret_key = get_or_create_secret()?;
    crate::core::options::apply_bind_addrs(
        iroh::Endpoint::builder(presets::Minimal)
            .alpns(alpns)
            .secret_key(secret_key)
            .relay_mode(options.relay_mode().into()),
        options,
    )
}
