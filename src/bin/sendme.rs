//! 二进制入口：使用 `sendmer::cli` 启动命令行程序。
//!
//! 该文件仅包含最小的启动逻辑：初始化日志并调用 `cli::run()`。
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
