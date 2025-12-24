use sendmer::cli;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let res = cli::run().await;

    if let Err(e) = &res {
        eprintln!("{e}");
    }

    match res {
        Ok(()) => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}
