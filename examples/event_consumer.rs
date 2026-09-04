use sendmer::{
    AppHandle, EventEmitter, SendOptions, TransferEvent, TransferEventStreamValidator, send_handle,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

struct JsonEventLog {
    validator: Mutex<TransferEventStreamValidator>,
}

impl EventEmitter for JsonEventLog {
    fn emit(&self, event: &TransferEvent) {
        let mut validator = self.validator.lock().expect("event validator lock");
        if let Err(error) = validator.accept(event) {
            eprintln!("rejected transfer event stream: {error}");
            return;
        }
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
    let events: AppHandle = Some(Arc::new(JsonEventLog {
        validator: Mutex::new(TransferEventStreamValidator::new()),
    }));
    let handle = send_handle(PathBuf::from(path), SendOptions::default(), events).await?;

    println!("sendmer receive {}", handle.ticket());
    tokio::signal::ctrl_c().await?;
    handle.cancel().await
}
