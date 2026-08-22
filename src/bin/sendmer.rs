//! 二进制入口：使用 `sendmer::main()` 启动命令行程序。
//!
//! 该文件仅包含最小的启动逻辑：初始化日志并调用 `run()`。

use clap::error::{ContextKind, ErrorKind};
use clap::{CommandFactory, Parser};
use console::style;
use data_encoding::HEXLOWER;
use indicatif::HumanBytes;
#[cfg(feature = "clipboard")]
use n0_future::StreamExt;
use sendmer::core::args::{
    Args, CacheArgs, CacheCommands, Commands, CommonArgs, ReceiveArgs, SendArgs,
    get_or_create_secret, print_hash,
};
use sendmer::core::cli_helper::CliEventEmitter;
use sendmer::core::results::SenderTransferStatus;
use sendmer::core::{receiver, sender};
use sendmer::{AppHandle, ReceiveCacheOptions, ReceiveOptions, SendOptions};
#[cfg(feature = "clipboard")]
use std::io::IsTerminal;
use std::sync::Arc;
use std::{future::Future, io};

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

    if let Some(common) = common_args(&args.command) {
        init_tracing(common.verbose)?;
        maybe_show_secret(common)?;
    } else {
        init_tracing(0)?;
    }

    match args.command {
        Commands::Send(args) => send(args).await,
        Commands::Receive(args) => receive(args).await,
        Commands::Cache(args) => cache(args).await,
    }
}

/// Run cache maintenance through the same public API available to GUI clients.
async fn cache(args: CacheArgs) -> anyhow::Result<()> {
    match args.command {
        CacheCommands::Prune(args) => {
            let report = sendmer::prune_receive_cache(args.cache_dir).await?;
            println!(
                "Removed {} expired entries; retained {}, active {}, unknown {}",
                report.removed_entries,
                report.retained_entries,
                report.active_entries,
                report.unknown_entries
            );
            Ok(())
        }
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
    let app_handle = cli_app_handle("[send]", args.common.no_progress, args.common.json_events);

    let res = sender::send(args.path.clone(), opts, app_handle).await?;

    print_command_output(
        args.common.json_events,
        format!(
            "imported {} {}, {}, hash {}",
            res.entry_type,
            args.path.display(),
            HumanBytes(res.size),
            print_hash(&res.hash, args.common.format)
        ),
    );

    print_command_output(args.common.json_events, "to get this data, use");
    print_command_output(
        args.common.json_events,
        format!("sendmer receive {}", res.ticket),
    );
    #[cfg(feature = "clipboard")]
    if !args.common.json_events {
        maybe_handle_key_press(args.clipboard, res.ticket.to_string());
    }
    let wait_result = wait_for_send_shutdown(&res).await;
    let shutdown_result = res.cancel().await;
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
    let opts = receive_options(&args);
    let app_handle = cli_app_handle("[recv]", args.common.no_progress, args.common.json_events);

    let res = receiver::receive(args.ticket.to_string(), opts, app_handle).await?;
    print_command_output(
        args.common.json_events,
        format!("{} in {:?}", res.message, res.file_path),
    );
    Ok(())
}

/// Keep machine-readable JSONL on stdout while routing human text to stderr.
fn print_command_output(json_events: bool, message: impl std::fmt::Display) {
    if json_events {
        eprintln!("{message}");
    } else {
        println!("{message}");
    }
}

fn send_options(args: &SendArgs) -> SendOptions {
    SendOptions {
        relay_mode: args.common.relay.clone(),
        ticket_type: args.ticket_type,
        magic_ipv4_addr: args.common.magic_ipv4_addr,
        magic_ipv6_addr: args.common.magic_ipv6_addr,
        max_upload_rate_bytes_per_sec: args.max_upload_rate,
        max_receivers: args.max_receivers,
    }
}

fn receive_options(args: &ReceiveArgs) -> ReceiveOptions {
    ReceiveOptions {
        output_dir: args.output_dir.clone(),
        relay_mode: args.common.relay.clone(),
        magic_ipv4_addr: args.common.magic_ipv4_addr,
        magic_ipv6_addr: args.common.magic_ipv6_addr,
        retry_policy: sendmer::core::options::ReceiveRetryPolicy {
            download_retry_limit: args.retry_limit,
            download_retry_backoff_ms: args.retry_backoff_ms,
            connect_timeout_ms: args.connect_timeout_ms,
            metadata_timeout_ms: args.metadata_timeout_ms,
            download_idle_timeout_ms: args.download_idle_timeout_ms,
            ..Default::default()
        },
        receive_cache: args.cache_dir.clone().map(|root_dir| {
            ReceiveCacheOptions::new(root_dir)
                .with_ttl(std::time::Duration::from_secs(args.cache_ttl_seconds))
        }),
    }
}

fn cli_app_handle(prefix: &'static str, no_progress: bool, json_events: bool) -> AppHandle {
    if json_events {
        Some(Arc::new(CliEventEmitter::json_lines()))
    } else if no_progress {
        None
    } else {
        Some(Arc::new(CliEventEmitter::new(prefix)))
    }
}

async fn wait_for_send_shutdown(res: &sendmer::core::results::SendResult) -> anyhow::Result<()> {
    let mut status_rx = res.subscribe_transfer_status();

    wait_for_send_shutdown_with_signal(tokio::signal::ctrl_c(), &mut status_rx).await
}

/// Wait for the user to stop sharing while treating receiver updates as telemetry.
///
/// A receiver can abort independently while other receivers still use the same ticket, so
/// status updates must not shut down the shared sender. Closing the status channel means the
/// sender is already being torn down elsewhere.
async fn wait_for_send_shutdown_with_signal<S>(
    signal: S,
    status_rx: &mut tokio::sync::watch::Receiver<SenderTransferStatus>,
) -> anyhow::Result<()>
where
    S: Future<Output = io::Result<()>>,
{
    tokio::pin!(signal);

    loop {
        tokio::select! {
            result = &mut signal => {
                result?;
                return Ok(());
            }
            changed = status_rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }

                // A single receiver can abort without invalidating the shared ticket.
                // The progress event reports that failure while this loop keeps serving.
            }
        }
    }
}

fn common_args(command: &Commands) -> Option<&CommonArgs> {
    match command {
        Commands::Send(args) => Some(&args.common),
        Commands::Receive(args) => Some(&args.common),
        Commands::Cache(_) => None,
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
        .with_writer(std::io::stderr)
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
    use super::{receive_options, send_options, wait_for_send_shutdown_with_signal};
    use clap::Parser;
    use iroh::EndpointAddr;
    use iroh_blobs::{BlobFormat, Hash, ticket::BlobTicket};
    use sendmer::core::args::{CommonArgs, ReceiveArgs};
    use sendmer::core::options::RelayModeOption;
    use sendmer::core::results::SenderTransferStatus;
    use sendmer::{Args, Commands};
    use std::path::PathBuf;

    fn sample_common_args() -> CommonArgs {
        CommonArgs {
            magic_ipv4_addr: None,
            magic_ipv6_addr: None,
            format: Default::default(),
            verbose: 0,
            no_progress: false,
            json_events: false,
            relay: RelayModeOption::Default,
            show_secret: false,
        }
    }

    fn sample_receive_args() -> ReceiveArgs {
        ReceiveArgs {
            ticket: BlobTicket::new(
                EndpointAddr::new(iroh::SecretKey::generate().public()),
                Hash::new(b"receive option mapping"),
                BlobFormat::HashSeq,
            ),
            output_dir: None,
            retry_limit: 3,
            retry_backoff_ms: 250,
            connect_timeout_ms: None,
            metadata_timeout_ms: None,
            download_idle_timeout_ms: None,
            cache_dir: None,
            cache_ttl_seconds: 604_800,
            common: sample_common_args(),
        }
    }

    #[test]
    fn receive_options_keeps_explicit_output_dir() {
        let output = Some(PathBuf::from("explicit-output"));
        let mut args = sample_receive_args();
        args.output_dir = output.clone();
        args.retry_limit = 5;
        args.retry_backoff_ms = 120;
        args.connect_timeout_ms = Some(1_000);
        args.metadata_timeout_ms = Some(2_000);
        args.download_idle_timeout_ms = Some(3_000);
        args.cache_dir = Some(PathBuf::from("receive-cache"));
        args.cache_ttl_seconds = 86_400;

        let options = receive_options(&args);

        assert_eq!(options.output_dir, output);
        assert_eq!(options.retry_policy.download_retry_limit, 5);
        assert_eq!(options.retry_policy.download_retry_backoff_ms, 120);
        assert_eq!(options.retry_policy.connect_timeout_ms, Some(1_000));
        assert_eq!(options.retry_policy.metadata_timeout_ms, Some(2_000));
        assert_eq!(options.retry_policy.download_idle_timeout_ms, Some(3_000));
        let cache = options.receive_cache.expect("persistent receive cache");
        assert_eq!(cache.root_dir, PathBuf::from("receive-cache"));
        assert_eq!(cache.ttl, std::time::Duration::from_secs(86_400));
    }

    #[test]
    fn receive_options_preserves_missing_output_dir() {
        let options = receive_options(&sample_receive_args());

        assert!(options.output_dir.is_none());
        assert!(options.receive_cache.is_none());
    }

    #[test]
    fn send_options_preserves_upload_rate() {
        let args = Args::try_parse_from([
            "sendmer",
            "send",
            "--max-upload-rate",
            "2048",
            "--max-receivers",
            "3",
            "example.bin",
        ])
        .expect("valid send arguments");
        let Commands::Send(send) = args.command else {
            panic!("expected send command")
        };

        let options = send_options(&send);
        assert_eq!(
            options
                .max_upload_rate_bytes_per_sec
                .map(std::num::NonZeroU64::get),
            Some(2_048)
        );
        assert_eq!(
            options.max_receivers.map(std::num::NonZeroU64::get),
            Some(3)
        );
    }

    #[tokio::test]
    async fn sender_wait_ignores_aborted_receiver_status() {
        let (status_tx, mut status_rx) = tokio::sync::watch::channel(SenderTransferStatus::Idle);
        status_tx
            .send(SenderTransferStatus::Aborted)
            .expect("status receiver should be present");
        let wait = wait_for_send_shutdown_with_signal(
            std::future::pending::<std::io::Result<()>>(),
            &mut status_rx,
        );
        tokio::pin!(wait);

        tokio::select! {
            biased;
            result = &mut wait => panic!("an aborted receiver must not stop the sender: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
    }
}
