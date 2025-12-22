use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};
use anyhow::Context;
use data_encoding::HEXLOWER;
use futures_buffered::BufferedStreamExt;
use indicatif::{HumanBytes, MultiProgress, ProgressDrawTarget};
use iroh::{Endpoint, RelayMode};
use iroh::discovery::pkarr::PkarrPublisher;
use iroh_blobs::{provider, BlobFormat, BlobsProtocol};
use iroh_blobs::api::{Store, TempTag};
use iroh_blobs::api::blobs::{AddPathOptions, AddProgressItem, ImportMode};
use iroh_blobs::format::collection::Collection;
use iroh_blobs::provider::events::{ConnectMode, EventMask, EventSender};
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::ticket::BlobTicket;
use n0_future::StreamExt;
use n0_future::task::AbortOnDropHandle;
use rand::Rng;
use tokio::sync::mpsc;
use tracing::trace;
use walkdir::WalkDir;
use crate::core::common::apply_options;
use crate::core::progress::{make_import_item_progress, make_import_overall_progress, show_provide_progress};
use crate::types::{print_hash, AddrInfoOptions, SendArgs};
use crate::utils::get_or_create_secret;

pub async fn send(args: SendArgs) -> anyhow::Result<()> {
    let secret_key = get_or_create_secret(args.common.verbose > 0)?;
    if args.common.show_secret {
        let secret_key = hex::encode(secret_key.to_bytes());
        eprintln!("using secret key {secret_key}");
    }
    // create a magicsocket endpoint
    let relay_mode: RelayMode = args.common.relay.into();
    let mut builder = Endpoint::builder()
        .alpns(vec![iroh_blobs::protocol::ALPN.to_vec()])
        .secret_key(secret_key)
        .relay_mode(relay_mode.clone());
    if args.ticket_type == AddrInfoOptions::Id {
        builder = builder.discovery(PkarrPublisher::n0_dns());
    }
    if let Some(addr) = args.common.magic_ipv4_addr {
        builder = builder.bind_addr_v4(addr);
    }
    if let Some(addr) = args.common.magic_ipv6_addr {
        builder = builder.bind_addr_v6(addr);
    }

    // use a flat store - todo: use a partial in mem store instead
    let suffix = rand::rng().random::<[u8; 16]>();
    let cwd = std::env::current_dir()?;
    let blobs_data_dir = cwd.join(format!(".sendmer-send-{}", HEXLOWER.encode(&suffix)));
    if blobs_data_dir.exists() {
        println!(
            "can not share twice from the same directory: {}",
            cwd.display(),
        );
        std::process::exit(1);
    }
    // todo: remove this as soon as we have a mem store that does not require a temp dir,
    // or create a temp dir outside the current directory.
    if cwd.join(&args.path) == cwd {
        println!("can not share from the current directory");
        std::process::exit(1);
    }

    let mut mp = MultiProgress::new();
    let mp2 = mp.clone();
    let path = args.path;
    let path2 = path.clone();
    let blobs_data_dir2 = blobs_data_dir.clone();
    let (progress_tx, progress_rx) = mpsc::channel(32);
    let progress = AbortOnDropHandle::new(n0_future::task::spawn(show_provide_progress(
        mp2,
        progress_rx,
    )));
    let setup = async move {
        let t0 = Instant::now();
        tokio::fs::create_dir_all(&blobs_data_dir2).await?;

        let endpoint = builder.bind().await?;
        let draw_target = if args.common.no_progress {
            ProgressDrawTarget::hidden()
        } else {
            ProgressDrawTarget::stderr()
        };
        mp.set_draw_target(draw_target);
        let store = FsStore::load(&blobs_data_dir2).await?;
        let blobs = BlobsProtocol::new(
            &store,
            Some(EventSender::new(
                progress_tx,
                EventMask {
                    connected: ConnectMode::Notify,
                    get: provider::events::RequestMode::NotifyLog,
                    ..EventMask::DEFAULT
                },
            )),
        );

        let import_result = import(path2, blobs.store(), &mut mp).await?;
        let dt = t0.elapsed();

        let router = iroh::protocol::Router::builder(endpoint)
            .accept(iroh_blobs::ALPN, blobs.clone())
            .spawn();

        // wait for the endpoint to figure out its address before making a ticket
        let ep = router.endpoint();
        tokio::time::timeout(Duration::from_secs(30), async move {
            if !matches!(relay_mode, RelayMode::Disabled) {
                let _ = ep.online().await;
            }
        })
            .await?;

        anyhow::Ok((router, import_result, dt))
    };
    let (router, (temp_tag, size, collection), dt) = tokio::select! {
        x = setup => x?,
        _ = tokio::signal::ctrl_c() => {
            std::process::exit(130);
        }
    };
    let hash = temp_tag.hash();

    // make a ticket
    let mut addr = router.endpoint().addr();
    apply_options(&mut addr, args.ticket_type);
    let ticket = BlobTicket::new(addr, hash, BlobFormat::HashSeq);
    let entry_type = if path.is_file() { "file" } else { "directory" };
    println!(
        "imported {} {}, {}, hash {}",
        entry_type,
        path.display(),
        HumanBytes(size),
        print_hash(&hash, args.common.format),
    );
    if args.common.verbose > 1 {
        for (name, hash) in collection.iter() {
            println!("    {} {name}", print_hash(hash, args.common.format));
        }
        println!(
            "{}s, {}/s",
            dt.as_secs_f64(),
            HumanBytes(((size as f64) / dt.as_secs_f64()).floor() as u64)
        );
    }

    println!("to get this data, use");
    println!("sendmer receive {ticket}");

    #[cfg(feature = "clipboard")]
    handle_key_press(args.clipboard, ticket);

    tokio::signal::ctrl_c().await?;

    drop(temp_tag);

    println!("shutting down");
    tokio::time::timeout(Duration::from_secs(2), router.shutdown()).await??;
    tokio::fs::remove_dir_all(blobs_data_dir).await?;
    // drop everything that owns blobs to close the progress sender
    drop(router);
    // await progress completion so the progress bar is cleared
    progress.await.ok();

    Ok(())
}


/// Import from a file or directory into the database.
///
/// The returned tag always refers to a collection. If the input is a file, this
/// is a collection with a single blob, named like the file.
///
/// If the input is a directory, the collection contains all the files in the
/// directory.
async fn import(
    path: PathBuf,
    db: &Store,
    mp: &mut MultiProgress,
) -> anyhow::Result<(TempTag, u64, Collection)> {
    let parallelism = num_cpus::get();
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root = path.parent().context("context get parent")?;
    // walkdir also works for files, so we don't need to special case them
    let files = WalkDir::new(path.clone()).into_iter();
    // flatten the directory structure into a list of (name, path) pairs.
    // ignore symlinks.
    let data_sources: Vec<(String, PathBuf)> = files
        .map(|entry| {
            let entry = entry?;
            if !entry.file_type().is_file() {
                // Skip symlinks. Directories are handled by WalkDir.
                return Ok(None);
            }
            let path = entry.into_path();
            let relative = path.strip_prefix(root)?;
            let name = canonicalized_path_to_string(relative, true)?;
            anyhow::Ok(Some((name, path)))
        })
        .filter_map(Result::transpose)
        .collect::<anyhow::Result<Vec<_>>>()?;
    // import all the files, using num_cpus workers, return names and temp tags
    let op = mp.add(make_import_overall_progress());
    op.set_message(format!("importing {} files", data_sources.len()));
    op.set_length(data_sources.len() as u64);
    let mut names_and_tags = n0_future::stream::iter(data_sources)
        .map(|(name, path)| {
            let db = db.clone();
            let op = op.clone();
            let mp = mp.clone();
            async move {
                op.inc(1);
                let pb = mp.add(make_import_item_progress());
                pb.set_message(format!("copying {name}"));
                let import = db.add_path_with_opts(AddPathOptions {
                    path,
                    mode: ImportMode::TryReference,
                    format: BlobFormat::Raw,
                });
                let mut stream = import.stream().await;
                let mut item_size = 0;
                let temp_tag = loop {
                    let item = stream
                        .next()
                        .await
                        .context("import stream ended without a tag")?;
                    trace!("importing {name} {item:?}");
                    match item {
                        AddProgressItem::Size(size) => {
                            item_size = size;
                            pb.set_length(size);
                        }
                        AddProgressItem::CopyProgress(offset) => {
                            pb.set_position(offset);
                        }
                        AddProgressItem::CopyDone => {
                            pb.set_message(format!("computing outboard {name}"));
                            pb.set_position(0);
                        }
                        AddProgressItem::OutboardProgress(offset) => {
                            pb.set_position(offset);
                        }
                        AddProgressItem::Error(cause) => {
                            pb.finish_and_clear();
                            anyhow::bail!("error importing {}: {}", name, cause);
                        }
                        AddProgressItem::Done(tt) => {
                            pb.finish_and_clear();
                            break tt;
                        }
                    }
                };
                anyhow::Ok((name, temp_tag, item_size))
            }
        })
        .buffered_unordered(parallelism)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;
    op.finish_and_clear();
    names_and_tags.sort_by(|(a, _, _), (b, _, _)| a.cmp(b));
    // total size of all files
    let size = names_and_tags.iter().map(|(_, _, size)| *size).sum::<u64>();
    // collect the (name, hash) tuples into a collection
    // we must also keep the tags around so the data does not get gced.
    let (collection, tags) = names_and_tags
        .into_iter()
        .map(|(name, tag, _)| ((name, tag.hash()), tag))
        .unzip::<_, _, Collection, Vec<_>>();
    let temp_tag = collection.clone().store(db).await?;
    // now that the collection is stored, we can drop the tags
    // data is protected by the collection
    drop(tags);
    Ok((temp_tag, size, collection))
}

#[cfg(feature = "clipboard")]
fn handle_key_press(set_clipboard: bool, ticket: BlobTicket) {
    #[cfg(any(unix, windows))]
    use std::io;

    use crossterm::{
        event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
        terminal::{disable_raw_mode, enable_raw_mode},
    };
    #[cfg(unix)]
    use libc::{raise, SIGINT};
    #[cfg(windows)]
    use windows_sys::Win32::System::Console::{GenerateConsoleCtrlEvent, CTRL_C_EVENT};

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
fn add_to_clipboard(ticket: &BlobTicket) {
    use std::io::stdout;

    use crossterm::{clipboard::CopyToClipboard, execute};

    execute!(
        stdout(),
        CopyToClipboard::to_clipboard_from(format!("sendmer receive {ticket}"))
    )
        .unwrap_or_else(|e| eprintln!("Failed to copy to clipboard: {e}"));
}

/// This function converts an already canonicalized path to a string.
///
/// If `must_be_relative` is true, the function will fail if any component of the path is
/// `Component::RootDir`
///
/// This function will also fail if the path is non-canonical, i.e. contains
/// `..` or `.`, or if the path components contain any windows or unix path
/// separators.
pub fn canonicalized_path_to_string(
    path: impl AsRef<Path>,
    must_be_relative: bool,
) -> anyhow::Result<String> {
    let mut path_str = String::new();
    let parts = path
        .as_ref()
        .components()
        .filter_map(|c| match c {
            Component::Normal(x) => {
                let c = match x.to_str() {
                    Some(c) => c,
                    None => return Some(Err(anyhow::anyhow!("invalid character in path"))),
                };

                if !c.contains('/') && !c.contains('\\') {
                    Some(Ok(c))
                } else {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                }
            }
            Component::RootDir => {
                if must_be_relative {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                } else {
                    path_str.push('/');
                    None
                }
            }
            _ => Some(Err(anyhow::anyhow!("invalid path component {:?}", c))),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let parts = parts.join("/");
    path_str.push_str(&parts);
    Ok(path_str)
}