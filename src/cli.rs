use std::sync::Arc;
use crate::core::types::{Args, Commands, ReceiveArgs, SendArgs};
use crate::{SendOptions, ReceiveOptions, AppHandle};
use crate::core::progress::CliEventEmitter;
use indicatif::HumanBytes;
use clap::{
    CommandFactory, Parser,
    error::{ContextKind, ErrorKind},
};
use console::style;

pub async fn run() -> anyhow::Result<()> {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(cause) => {
            if let Some(text) = cause.get(ContextKind::InvalidSubcommand) {
                eprintln!("{} \"{}\"\n", ErrorKind::InvalidSubcommand, text);
                eprintln!("Available subcommands are");
                for cmd in Args::command().get_subcommands() {
                    eprintln!("    {}", style(cmd.get_name()).bold());
                }
                std::process::exit(1);
            } else {
                cause.exit();
            }
        }
    };

    match args.command {
        Commands::Send(args) => send(args).await,
        Commands::Receive(args) => receive(args).await,
    }
}

/// CLI wrapper: call library `start_share` and show minimal output.
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

    let res = crate::start_share(args.path.clone(), opts, app_handle).await?;

    println!(
        "imported {} {}, {}, hash {}",
        res.entry_type,
        args.path.display(),
        HumanBytes(res.size),
        res.hash 
    );

    println!("to get this data, use");
    println!("sendmer receive {}", res.ticket);

    tokio::signal::ctrl_c().await?;

    drop(res.temp_tag);
    tokio::time::timeout(std::time::Duration::from_secs(2), res.router.shutdown()).await??;
    tokio::fs::remove_dir_all(res.blobs_data_dir).await?;
    drop(res.router);

    Ok(())
}

/// CLI wrapper: call library `download` and print the result message.
pub async fn receive(args: ReceiveArgs) -> anyhow::Result<()> {

    let opts = ReceiveOptions {
        output_dir: None,
        relay_mode: args.common.relay,
        magic_ipv4_addr: args.common.magic_ipv4_addr,
        magic_ipv6_addr: args.common.magic_ipv6_addr,
    };

    let app_handle: AppHandle = if args.common.no_progress {
        None
    } else {
        Some(Arc::new(CliEventEmitter::new("[recv]")))
    };

    let res = crate::download(args.ticket.to_string(), opts, app_handle).await?;
    println!("{}", res.message);
    Ok(())
}
