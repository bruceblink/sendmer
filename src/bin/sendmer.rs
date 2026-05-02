//! 二进制入口：使用 `sendmer::main()` 启动命令行程序。
//!
//! 该文件仅包含最小的启动逻辑：初始化日志并调用 `run()`。

use clap::error::{ContextKind, ErrorKind};
use clap::{CommandFactory, Parser};
use console::style;
use data_encoding::HEXLOWER;
use indicatif::HumanBytes;
use n0_future::StreamExt;
use sendmer::core::args::{
    Args, Commands, CommonArgs, ReceiveArgs, SendArgs, get_or_create_secret, print_hash,
};
use sendmer::core::cli_helper::CliEventEmitter;
use sendmer::core::results::SenderTransferStatus;
use sendmer::core::{receiver, sender};
use sendmer::{AppHandle, ReceiveOptions, SendOptions};
use std::io::IsTerminal;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    let res = run().await;

    if let Err(e) = &res {
        eprintln!("{e}");
    }

    match res {
        Ok(()) => std::process::exit(0),
        Err(_) => std::process::exit(1),
    }
}

/// 处理 CLI 参数并分发到具体子命令处理函数。
///
/// 该函数负责解析 `Args` 并调用 `send` 或 `receive`。
pub async fn run() -> anyhow::Result<()> {
    let args = Args::try_parse().unwrap_or_else(|cause| {
        cause.get(ContextKind::InvalidSubcommand).map_or_else(
            || {
                cause.exit();
            },
            |text| {
                eprintln!("{} \"{}\"\n", ErrorKind::InvalidSubcommand, text);
                eprintln!("Available subcommands are");
                for cmd in Args::command().get_subcommands() {
                    eprintln!("    {}", style(cmd.get_name()).bold());
                }
                std::process::exit(1);
            },
        )
    });

    init_tracing(common_args(&args.command).verbose)?;
    maybe_show_secret(common_args(&args.command))?;

    match args.command {
        Commands::Send(args) => send(args).await,
        Commands::Receive(args) => receive(args).await,
    }
}

/// CLI wrapper: call library `start_share` and show minimal output.
///
/// 该函数为 `send` 子命令提供一个小封装：构建 `SendOptions`，
/// 根据 `args.common.no_progress` 决定是否启用 `CliEventEmitter`，
/// 启动分享并在完成后清理临时资源。
///
/// 该函数主要用于命令行程序，不作为库 API 的一部分使用。
async fn send(args: SendArgs) -> anyhow::Result<()> {
    let opts = send_options(&args);
    let app_handle = cli_app_handle("[send]", args.common.no_progress);

    let res = sender::send(args.path.clone(), opts, app_handle).await?;

    println!(
        "imported {} {}, {}, hash {}",
        res.entry_type,
        args.path.display(),
        HumanBytes(res.size),
        print_hash(&res.hash, args.common.format)
    );

    println!("to get this data, use");
    println!("sendmer receive {}", res.ticket);
    #[cfg(feature = "clipboard")]
    maybe_handle_key_press(args.clipboard, res.ticket.to_string());
    let wait_result = wait_for_send_shutdown(&res).await;
    let shutdown_result = res.shutdown().await;
    match (wait_result, shutdown_result) {
        (Err(error), Err(shutdown_error)) => {
            tracing::warn!(error = %shutdown_error, "failed to shutdown sender after wait error");
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(()), shutdown_result) => shutdown_result,
    }
}

/// CLI wrapper: call library `download` and print the result message.
///
/// 与 `send` 类似，`receive` 在命令行模式下决定是否创建 `CliEventEmitter`，
/// 调用 `download` 并将结果消息输出到 stdout。
async fn receive(args: ReceiveArgs) -> anyhow::Result<()> {
    let opts = receive_options(args.output_dir.clone(), &args.common);
    let app_handle = cli_app_handle("[recv]", args.common.no_progress);

    let res = receiver::receive(args.ticket.to_string(), opts, app_handle).await?;
    println!("{} in {:?}", res.message, res.file_path);
    Ok(())
}

fn send_options(args: &SendArgs) -> SendOptions {
    SendOptions {
        relay_mode: args.common.relay.clone(),
        ticket_type: args.ticket_type,
        magic_ipv4_addr: args.common.magic_ipv4_addr,
        magic_ipv6_addr: args.common.magic_ipv6_addr,
    }
}

fn receive_options(output_dir: Option<std::path::PathBuf>, common: &CommonArgs) -> ReceiveOptions {
    ReceiveOptions {
        output_dir,
        relay_mode: common.relay.clone(),
        magic_ipv4_addr: common.magic_ipv4_addr,
        magic_ipv6_addr: common.magic_ipv6_addr,
        retry_policy: Default::default(),
    }
}

fn cli_app_handle(prefix: &'static str, no_progress: bool) -> AppHandle {
    if no_progress {
        None
    } else {
        Some(Arc::new(CliEventEmitter::new(prefix)))
    }
}

async fn wait_for_send_shutdown(res: &sendmer::core::results::SendResult) -> anyhow::Result<()> {
    let mut status_rx = res.subscribe_transfer_status();

    loop {
        if matches!(*status_rx.borrow(), SenderTransferStatus::Aborted) {
            anyhow::bail!("receiver cancelled the transfer");
        }

        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                result?;
                return Ok(());
            }
            changed = status_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }

                match *status_rx.borrow() {
                    SenderTransferStatus::Aborted => {
                        anyhow::bail!("receiver cancelled the transfer");
                    }
                    SenderTransferStatus::Idle
                    | SenderTransferStatus::Started
                    | SenderTransferStatus::Completed => {}
                }
            }
        }
    }
}

fn common_args(command: &Commands) -> &CommonArgs {
    match command {
        Commands::Send(args) => &args.common,
        Commands::Receive(args) => &args.common,
    }
}

fn init_tracing(verbose: u8) -> anyhow::Result<()> {
    let default_filter = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .or_else(|_| tracing_subscriber::EnvFilter::try_new(default_filter))?;

    let _ = tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .try_init();
    Ok(())
}

fn maybe_show_secret(common: &CommonArgs) -> anyhow::Result<()> {
    if common.show_secret {
        let secret = get_or_create_secret()?;
        eprintln!("Secret: {}", HEXLOWER.encode(&secret.to_bytes()));
    }
    Ok(())
}

#[cfg(feature = "clipboard")]
fn maybe_handle_key_press(set_clipboard: bool, ticket: String) {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        return;
    }
    handle_key_press(set_clipboard, ticket);
}

#[cfg(feature = "clipboard")]
fn handle_key_press(set_clipboard: bool, ticket: String) {
    #[cfg(any(unix, windows))]
    use std::io;

    use crossterm::{
        event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        terminal::{disable_raw_mode, enable_raw_mode},
    };
    #[cfg(unix)]
    use libc::{SIGINT, raise};
    #[cfg(windows)]
    use windows_sys::Win32::System::Console::{CTRL_C_EVENT, GenerateConsoleCtrlEvent};

    struct RawModeGuard;

    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
        }
    }

    if set_clipboard {
        add_to_clipboard(&ticket);
    }

    let _keyboard = tokio::task::spawn(async move {
        println!("press c to copy command to clipboard, or use the --clipboard argument");

        let _raw_mode_guard = match enable_raw_mode() {
            Ok(()) => Some(RawModeGuard),
            Err(err) => {
                eprintln!("Failed to enable raw mode: {err}");
                None
            }
        };

        EventStream::new()
            .for_each(move |e| match e {
                Err(err) => eprintln!("Failed to process event: {err}"),
                Ok(Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    ..
                })) => add_to_clipboard(&ticket),
                Ok(Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    kind: KeyEventKind::Press,
                    ..
                })) => {
                    let _ = disable_raw_mode();

                    #[cfg(unix)]
                    if unsafe { raise(SIGINT) } != 0 {
                        eprintln!("Failed to raise signal: {}", io::Error::last_os_error());
                    }

                    #[cfg(windows)]
                    if unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) } == 0 {
                        eprintln!(
                            "Failed to generate console event: {}",
                            io::Error::last_os_error()
                        );
                    }
                }
                _ => {}
            })
            .await;
    });
}

#[cfg(feature = "clipboard")]
fn add_to_clipboard(ticket: &String) {
    use std::io::stdout;

    use crossterm::{clipboard::CopyToClipboard, execute};

    execute!(
        stdout(),
        CopyToClipboard::to_clipboard_from(format!("sendmer receive {ticket}"))
    )
    .unwrap_or_else(|e| eprintln!("Failed to copy to clipboard: {e}"));
}

#[cfg(test)]
mod tests {
    use super::receive_options;
    use sendmer::core::args::CommonArgs;
    use sendmer::core::options::RelayModeOption;
    use std::path::PathBuf;

    fn sample_common_args() -> CommonArgs {
        CommonArgs {
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
            format: Default::default(),
            verbose: 0,
            no_progress: false,
            relay: RelayModeOption::Default,
            show_secret: false,
        }
    }

    #[test]
    fn receive_options_keeps_explicit_output_dir() {
        let common = sample_common_args();
        let output = Some(PathBuf::from("explicit-output"));

        let options = receive_options(output.clone(), &common);

        assert_eq!(options.output_dir, output);
    }

    #[test]
    fn receive_options_preserves_missing_output_dir() {
        let common = sample_common_args();

        let options = receive_options(None, &common);

        assert!(options.output_dir.is_none());
    }
}
