use crate::core::cli_helper::CliEventEmitter;
use crate::core::types::{Args, Commands, ReceiveArgs, SendArgs};
use crate::{AppHandle, ReceiveOptions, SendOptions};
use clap::{
    CommandFactory, Parser,
    error::{ContextKind, ErrorKind},
};
use console::style;
use indicatif::HumanBytes;
use n0_future::StreamExt;
use std::sync::Arc;

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
pub async fn send(args: SendArgs) -> anyhow::Result<()> {
    let opts = SendOptions {
        relay_mode: args.common.relay,
        ticket_type: args.ticket_type,
        magic_ipv4_addr: args.common.magic_ipv4_addr,
        magic_ipv6_addr: args.common.magic_ipv6_addr,
    };

    let app_handle: AppHandle = if args.common.no_progress {
        None
    } else {
        Some(Arc::new(CliEventEmitter::new("[send]")))
    };

    let res = crate::send(args.path.clone(), opts, app_handle).await?;

    println!(
        "imported {} {}, {}, hash {}",
        res.entry_type,
        args.path.display(),
        HumanBytes(res.size),
        res.hash
    );

    println!("to get this data, use");
    println!("sendmer receive {}", res.ticket);
    #[cfg(feature = "clipboard")]
    handle_key_press(args.clipboard, res.ticket);
    tokio::signal::ctrl_c().await?;

    drop(res.temp_tag);
    tokio::time::timeout(std::time::Duration::from_secs(2), res.router.shutdown()).await??;
    tokio::fs::remove_dir_all(res.blobs_data_dir).await?;
    drop(res.router);

    Ok(())
}

/// CLI wrapper: call library `download` and print the result message.
///
/// 与 `send` 类似，`receive` 在命令行模式下决定是否创建 `CliEventEmitter`，
/// 调用 `download` 并将结果消息输出到 stdout。
pub async fn receive(args: ReceiveArgs) -> anyhow::Result<()> {
    let opts = ReceiveOptions {
        output_dir: Option::from(std::env::current_dir()?),
        relay_mode: args.common.relay,
        magic_ipv4_addr: args.common.magic_ipv4_addr,
        magic_ipv6_addr: args.common.magic_ipv6_addr,
    };

    let app_handle: AppHandle = if args.common.no_progress {
        None
    } else {
        Some(Arc::new(CliEventEmitter::new("[recv]")))
    };

    let res = crate::receive(args.ticket.to_string(), opts, app_handle).await?;
    println!("{} in {:?}", res.message, res.file_path);
    Ok(())
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

    if set_clipboard {
        add_to_clipboard(&ticket);
    }

    let _keyboard = tokio::task::spawn(async move {
        println!("press c to copy command to clipboard, or use the --clipboard argument");

        // `enable_raw_mode` will remember the current terminal mode
        // and restore it when `disable_raw_mode` is called.
        enable_raw_mode().unwrap_or_else(|err| eprintln!("Failed to enable raw mode: {err}"));
        EventStream::new()
            .for_each(move |e| match e {
                Err(err) => eprintln!("Failed to process event: {err}"),
                // c is pressed
                Ok(Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::NONE,
                    kind: KeyEventKind::Press,
                    ..
                })) => add_to_clipboard(&ticket),
                // Ctrl+c is pressed
                Ok(Event::Key(KeyEvent {
                    code: KeyCode::Char('c'),
                    modifiers: KeyModifiers::CONTROL,
                    kind: KeyEventKind::Press,
                    ..
                })) => {
                    disable_raw_mode()
                        .unwrap_or_else(|e| eprintln!("Failed to disable raw mode: {e}"));

                    #[cfg(unix)]
                    // Safety: Raw syscall to re-send the SIGINT signal to the console.
                    // `raise` returns nonzero for failure.
                    if unsafe { raise(SIGINT) } != 0 {
                        eprintln!("Failed to raise signal: {}", io::Error::last_os_error());
                    }

                    #[cfg(windows)]
                    // Safety: Raw syscall to re-send the `CTRL_C_EVENT` to the console.
                    // `GenerateConsoleCtrlEvent` returns 0 for failure.
                    if unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) } == 0 {
                        eprintln!(
                            "Failed to generate console event: {}",
                            io::Error::last_os_error()
                        );
                    }
                }
                _ => {}
            })
            .await
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
