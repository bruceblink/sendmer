use super::{receive_limits, receive_options, send_options, wait_for_send_shutdown_with_signal};
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
        max_files: None,
        max_total_size: None,
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
    args.max_files = std::num::NonZeroU64::new(4);
    args.max_total_size = std::num::NonZeroU64::new(4_096);

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
    let limits = receive_limits(&args);
    assert_eq!(limits.max_files.map(std::num::NonZeroU64::get), Some(4));
    assert_eq!(
        limits.max_total_size_bytes.map(std::num::NonZeroU64::get),
        Some(4_096)
    );
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
        "--max-files",
        "10",
        "--max-total-size",
        "4096",
        "--max-import-memory",
        "65536",
        "--session-lifetime-seconds",
        "30",
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
    assert_eq!(options.max_files.map(std::num::NonZeroU64::get), Some(10));
    assert_eq!(
        options.max_total_size_bytes.map(std::num::NonZeroU64::get),
        Some(4_096)
    );
    assert_eq!(
        options
            .max_import_memory_bytes
            .map(std::num::NonZeroU64::get),
        Some(65_536)
    );
    assert_eq!(
        options.session_lifetime,
        Some(std::time::Duration::from_secs(30))
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

#[tokio::test]
async fn sender_wait_stops_when_session_expires() {
    let (status_tx, mut status_rx) = tokio::sync::watch::channel(SenderTransferStatus::Idle);
    status_tx
        .send(SenderTransferStatus::Expired)
        .expect("status receiver should be present");

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        wait_for_send_shutdown_with_signal(
            std::future::pending::<std::io::Result<()>>(),
            &mut status_rx,
        ),
    )
    .await
    .expect("expired sessions should stop the CLI wait")
    .expect("expired session wait should be clean");
}

#[tokio::test]
async fn sender_wait_stops_when_expiration_precedes_subscription() {
    let (status_tx, status_rx) = tokio::sync::watch::channel(SenderTransferStatus::Idle);
    status_tx
        .send(SenderTransferStatus::Expired)
        .expect("status receiver should be present");
    drop(status_rx);
    let status_rx = status_tx.subscribe();
    let mut status_rx = status_rx;

    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        wait_for_send_shutdown_with_signal(
            std::future::pending::<std::io::Result<()>>(),
            &mut status_rx,
        ),
    )
    .await
    .expect("expired subscription should stop the CLI wait")
    .expect("expired subscription wait should be clean");
}
