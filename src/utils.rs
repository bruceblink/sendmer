use std::str::FromStr;
use anyhow::Context;
use iroh::SecretKey;


/// Get the secret key or generate a new one.
///
/// Print the secret key to stderr if it was generated, so the user can save it.
pub fn get_or_create_secret(print: bool) -> anyhow::Result<SecretKey> {
    match std::env::var("IROH_SECRET") {
        Ok(secret) => SecretKey::from_str(&secret).context("invalid secret"),
        Err(_) => {
            let key = SecretKey::generate(&mut rand::rng());
            if print {
                let key = hex::encode(key.to_bytes());
                eprintln!("using secret key {key}");
            }
            Ok(key)
        }
    }
}