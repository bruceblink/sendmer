use std::path::{Path, PathBuf};
use console::style;
use indicatif::{HumanBytes, HumanDuration, MultiProgress, ProgressDrawTarget};
use iroh::discovery::dns::DnsDiscovery;
use iroh::Endpoint;
use iroh_blobs::api::blobs::{ExportMode, ExportOptions, ExportProgressItem};
use iroh_blobs::api::remote::GetProgressItem;
use iroh_blobs::api::Store;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::get::request::get_hash_seq_and_sizes;
use iroh_blobs::get::{GetError, Stats};
use iroh_blobs::store::fs::FsStore;
use n0_future::StreamExt;
use tokio::sync::mpsc;
use tracing::trace;
use crate::core::{make_connect_progress, make_download_progress, make_export_item_progress, make_export_overall_progress, make_get_sizes_progress};
use crate::types::{print_hash, ReceiveArgs};
use crate::utils::get_or_create_secret;

pub async fn receive(args: ReceiveArgs) -> anyhow::Result<()> {
    let ticket = args.ticket;
    let addr = ticket.addr().clone();
    let secret_key = get_or_create_secret(args.common.verbose > 0)?;
    let mut builder = Endpoint::builder()
        .alpns(vec![])
        .secret_key(secret_key)
        .relay_mode(args.common.relay.into());

    if ticket.addr().relay_urls().next().is_none() && ticket.addr().ip_addrs().next().is_none() {
        builder = builder.discovery(DnsDiscovery::n0_dns());
    }
    if let Some(addr) = args.common.magic_ipv4_addr {
        builder = builder.bind_addr_v4(addr);
    }
    if let Some(addr) = args.common.magic_ipv6_addr {
        builder = builder.bind_addr_v6(addr);
    }
    let endpoint = builder.bind().await?;
    let dir_name = format!(".sendmer-recv-{}", ticket.hash().to_hex());
    let iroh_data_dir = std::env::current_dir()?.join(dir_name);
    let db = FsStore::load(&iroh_data_dir).await?;
    let db2 = db.clone();
    trace!("load done!");
    let fut = async move {
        trace!("running");
        let mut mp: MultiProgress = MultiProgress::new();
        let draw_target = if args.common.no_progress {
            ProgressDrawTarget::hidden()
        } else {
            ProgressDrawTarget::stderr()
        };
        mp.set_draw_target(draw_target);
        let hash_and_format = ticket.hash_and_format();
        trace!("computing local");
        let local = db.remote().local(hash_and_format).await?;
        trace!("local done");
        let (stats, total_files, payload_size) = if !local.is_complete() {
            trace!("{} not complete", hash_and_format.hash);
            let cp = mp.add(make_connect_progress());
            let connection = endpoint.connect(addr, iroh_blobs::protocol::ALPN).await?;
            cp.finish_and_clear();
            let sp = mp.add(make_get_sizes_progress());
            let (_hash_seq, sizes) =
                get_hash_seq_and_sizes(&connection, &hash_and_format.hash, 1024 * 1024 * 32, None)
                    .await
                    .map_err(show_get_error)?;
            sp.finish_and_clear();
            let total_size = sizes.iter().copied().sum::<u64>();
            let payload_size = sizes.iter().skip(2).copied().sum::<u64>();
            let total_files = sizes.len().saturating_sub(1) as u64;
            eprintln!(
                "getting collection {} {} files, {}",
                print_hash(&ticket.hash(), args.common.format),
                total_files,
                HumanBytes(payload_size)
            );
            // print the details of the collection only in verbose mode
            if args.common.verbose > 0 {
                eprintln!(
                    "getting {} blobs in total, {}",
                    total_files + 1,
                    HumanBytes(total_size)
                );
            }
            let (tx, rx) = mpsc::channel(32);
            let local_size = local.local_bytes();
            let get = db.remote().execute_get(connection, local.missing());
            let task = tokio::spawn(show_download_progress(
                mp.clone(),
                rx,
                local_size,
                total_size,
            ));
            // let mut stream = get.stream();
            let mut stats = Stats::default();
            let mut stream = get.stream();
            while let Some(item) = stream.next().await {
                trace!("got item {item:?}");
                match item {
                    GetProgressItem::Progress(offset) => {
                        tx.send(offset).await.ok();
                    }
                    GetProgressItem::Done(value) => {
                        stats = value;
                        break;
                    }
                    GetProgressItem::Error(cause) => {
                        anyhow::bail!(show_get_error(cause));
                    }
                }
            }
            drop(tx);
            task.await.ok();
            (stats, total_files, payload_size)
        } else {
            println!("{} already complete", hash_and_format.hash);
            let total_files = local.children().unwrap() - 1;
            let payload_bytes = 0; // todo local.sizes().skip(2).map(Option::unwrap).sum::<u64>();
            (Stats::default(), total_files, payload_bytes)
        };
        let collection = Collection::load(hash_and_format.hash, db.as_ref()).await?;
        if args.common.verbose > 1 {
            for (name, hash) in collection.iter() {
                println!("    {} {name}", print_hash(hash, args.common.format));
            }
        }
        if let Some((name, _)) = collection.iter().next() {
            if let Some(first) = name.split('/').next() {
                println!("exporting to {first}");
            }
        }
        export(&db, collection, &mut mp).await?;
        anyhow::Ok((total_files, payload_size, stats))
    };
    let (total_files, payload_size, stats) = tokio::select! {
        x = fut => match x {
            Ok(x) => x,
            Err(e) => {
                // make sure we shutdown the db before exiting
                db2.shutdown().await?;
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        },
        _ = tokio::signal::ctrl_c() => {
            db2.shutdown().await?;
            std::process::exit(130);
        }
    };
    tokio::fs::remove_dir_all(iroh_data_dir).await?;
    if args.common.verbose > 0 {
        println!(
            "downloaded {} files, {}. took {} ({}/s)",
            total_files,
            HumanBytes(payload_size),
            HumanDuration(stats.elapsed),
            HumanBytes((stats.total_bytes_read() as f64 / stats.elapsed.as_secs_f64()) as u64),
        );
    }
    Ok(())
}

async fn export(db: &Store, collection: Collection, mp: &mut MultiProgress) -> anyhow::Result<()> {
    let root = std::env::current_dir()?;
    let op = mp.add(make_export_overall_progress());
    op.set_length(collection.len() as u64);
    for (i, (name, hash)) in collection.iter().enumerate() {
        op.set_position(i as u64);
        let target = get_export_path(&root, name)?;
        if target.exists() {
            eprintln!(
                "target {} already exists. Export stopped.",
                target.display()
            );
            eprintln!(
                "You can remove the file or directory and try again. The download will not be repeated."
            );
            anyhow::bail!("target {} already exists", target.display());
        }
        let mut stream = db
            .export_with_opts(ExportOptions {
                hash: *hash,
                target,
                mode: ExportMode::Copy,
            })
            .stream()
            .await;
        let pb = mp.add(make_export_item_progress());
        pb.set_message(format!("exporting {name}"));
        while let Some(item) = stream.next().await {
            match item {
                ExportProgressItem::Size(size) => {
                    pb.set_length(size);
                }
                ExportProgressItem::CopyProgress(offset) => {
                    pb.set_position(offset);
                }
                ExportProgressItem::Done => {
                    pb.finish_and_clear();
                }
                ExportProgressItem::Error(cause) => {
                    pb.finish_and_clear();
                    anyhow::bail!("error exporting {}: {}", name, cause);
                }
            }
        }
    }
    op.finish_and_clear();
    Ok(())
}

pub async fn show_download_progress(
    mp: MultiProgress,
    mut recv: mpsc::Receiver<u64>,
    local_size: u64,
    total_size: u64,
) -> anyhow::Result<()> {
    let op = mp.add(make_download_progress());
    op.set_length(total_size);
    while let Some(offset) = recv.recv().await {
        op.set_position(local_size + offset);
    }
    op.finish_and_clear();
    Ok(())
}

fn show_get_error(e: GetError) -> GetError {
    match &e {
        GetError::InitialNext { source, .. } => eprintln!(
            "{}",
            style(format!("initial connection error: {source}")).yellow()
        ),

        GetError::ConnectedNext { source, .. } => {
            eprintln!("{}", style(format!("connected error: {source}")).yellow())
        }
        GetError::AtBlobHeaderNext { source, .. } => eprintln!(
            "{}",
            style(format!("reading blob header error: {source}")).yellow()
        ),
        GetError::Decode { source, .. } => {
            eprintln!("{}", style(format!("decoding error: {source}")).yellow())
        }
        GetError::IrpcSend { source, .. } => eprintln!(
            "{}",
            style(format!("error sending over irpc: {source}")).yellow()
        ),
        GetError::AtClosingNext { source, .. } => {
            eprintln!("{}", style(format!("error at closing: {source}")).yellow())
        }
        GetError::BadRequest { .. } => eprintln!("{}", style("bad request").yellow()),
        GetError::LocalFailure { source, .. } => {
            eprintln!("{} {source:?}", style("local failure").yellow())
        }
    }
    e
}

fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let parts = name.split('/');
    let mut path = root.to_path_buf();
    for part in parts {
        validate_path_component(part)?;
        path.push(part);
    }
    Ok(path)
}

fn validate_path_component(component: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !component.contains('/'),
        "path components must not contain the only correct path separator, /"
    );
    Ok(())
}