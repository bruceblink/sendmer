use sendmer::{AppHandle, EventEmitter, SendOptions, TransferEvent, send_handle};
use std::path::PathBuf;
use std::sync::Arc;

struct JsonEventLog;

impl EventEmitter for JsonEventLog {
    fn emit(&self, event: &TransferEvent) {
        match serde_json::to_string(event) {
            Ok(json) => eprintln!("{json}"),
            Err(error) => eprintln!("failed to serialize transfer event: {error}"),
        }
    }
}

/// Share the path supplied on the command line until the user cancels the example.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Some(path) = std::env::args_os().nth(1) else {
        eprintln!("usage: cargo run --example event_consumer -- <path>");
        return Ok(());
    };
    let events: AppHandle = Some(Arc::new(JsonEventLog));
    let handle = send_handle(PathBuf::from(path), SendOptions::default(), events).await?;

    println!("sendmer receive {}", handle.ticket());
    tokio::signal::ctrl_c().await?;
    handle.cancel().await
}
