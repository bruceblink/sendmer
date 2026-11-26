//! Process endpoint identity shared by all transfer consumers.

use anyhow::Context;
use std::str::FromStr;
use std::sync::OnceLock;

static PROCESS_SECRET: OnceLock<iroh::SecretKey> = OnceLock::new();

/// Return the secret key used by the default local endpoint.
///
/// Honors IROH_SECRET when configured; otherwise returns the process-local
/// generated key. Treat the returned value as a credential. Never log it
/// implicitly or send it to remote peers.
pub fn endpoint_secret() -> anyhow::Result<iroh::SecretKey> {
    std::env::var("IROH_SECRET").map_or_else(
        |_| Ok(PROCESS_SECRET.get_or_init(new_secret_key).clone()),
        |secret| iroh::SecretKey::from_str(&secret).context("invalid secret"),
    )
}

fn new_secret_key() -> iroh::SecretKey {
    iroh::SecretKey::generate()
}
