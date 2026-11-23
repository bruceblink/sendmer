async fn prepare_env(
    ticket: &BlobTicket,
    options: &ReceiveOptions,
) -> anyhow::Result<(Endpoint, PathBuf, Store, Option<ReceiveCacheLease>)> {
    let mut builder = base_endpoint_builder(options, vec![]).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::InvalidInput,
            TransferPhase::Preparing,
            false,
            "invalid receive networking options",
        )
    })?;

    if ticket.addr().relay_urls().next().is_none() && ticket.addr().ip_addrs().next().is_none() {
        builder = builder.address_lookup(DnsAddressLookup::n0_dns());
    }
    let endpoint = builder.bind().await.map_err(|error| {
        receive_failure(
            error.into(),
            TransferErrorCode::ConnectionFailed,
            TransferPhase::Connecting,
            true,
            "unable to initialize receiver networking",
        )
    })?;

    let storage_result = options.receive_cache.as_ref().map_or_else(
        || {
            unique_temp_dir(&format!(
                "{RECEIVE_TEMP_DIR_PREFIX}{}-",
                ticket.hash().to_hex()
            ))
            .map(|path| (path, None))
        },
        |cache| {
            ReceiveCacheLease::open(cache, ticket.hash_and_format()).map(|lease| {
                let path = lease.entry_dir().to_path_buf();
                (path, Some(lease))
            })
        },
    );
    let (iroh_data_dir, cache_lease) = match storage_result {
        Ok(storage) => storage,
        Err(error) => {
            if let Err(close_error) = close_receive_endpoint(&endpoint).await {
                tracing::warn!(error = %close_error, "failed to close receiver endpoint after storage setup error");
            }
            return Err(receive_failure(
                error,
                TransferErrorCode::Filesystem,
                TransferPhase::Preparing,
                false,
                "unable to create receiver storage",
            ));
        }
    };
    let db = match load_fs_store(&iroh_data_dir).await {
        Ok(db) => db,
        Err(error) => {
            if let Err(close_error) = close_receive_endpoint(&endpoint).await {
                tracing::warn!(error = %close_error, "failed to close receiver endpoint after store setup error");
            }
            let cleanup_result = match cache_lease {
                Some(lease) => lease.preserve(),
                None => remove_temp_receive_dir(&iroh_data_dir).await,
            };
            if cleanup_result.is_err() {
                tracing::warn!("failed to finalize receiver storage after open error");
            }
            return Err(receive_failure(
                error,
                TransferErrorCode::Filesystem,
                TransferPhase::Preparing,
                false,
                "unable to open receiver storage",
            ));
        }
    };
    Ok((endpoint, iroh_data_dir, db.into(), cache_lease))
}

/// Fetch remote collection sizes with a fresh connection for each retry.
///
/// Reconnecting inside the attempt loop means an initial connection failure is
/// retried too, instead of skipping the retry policy before any request starts.
async fn get_sizes_with_retries(
    endpoint: &Endpoint,
    addr: &iroh::EndpointAddr,
    hash: &iroh_blobs::Hash,
    retry_policy: ReceiveRetryPolicy,
) -> anyhow::Result<(iroh_blobs::hashseq::HashSeq, StdArc<[u64]>)> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 1..=retry_policy.size_fetch_retry_limit {
        let connection = match await_receive_phase(
            retry_policy.connect_timeout(),
            TransferPhase::Connecting,
            "connect to sender for collection metadata",
            async {
                endpoint
                    .connect(addr.clone(), iroh_blobs::protocol::ALPN)
                    .await
                    .context("connect to sender for collection metadata")
            },
        )
        .await
        {
            Ok(connection) => connection,
            Err(error) => {
                let error = receive_failure(
                    error,
                    TransferErrorCode::ConnectionFailed,
                    TransferPhase::Connecting,
                    true,
                    "unable to connect to the sender",
                );
                tracing::error!(attempt, "collection metadata connection attempt failed");
                last_err = Some(error);
                if attempt < retry_policy.size_fetch_retry_limit {
                    tokio::time::sleep(size_fetch_backoff(attempt, retry_policy)).await;
                }
                continue;
            }
        };

        match await_receive_phase(
            retry_policy.metadata_timeout(),
            TransferPhase::Metadata,
            "fetch collection metadata",
            async {
                get_hash_seq_and_sizes(
                    &connection,
                    hash,
                    retry_policy
                        .size_fetch_chunk_size
                        .min(MAX_LEGACY_COLLECTION_METADATA_BYTES),
                    None,
                )
                .await
                .map_err(show_get_error)
                .map_err(anyhow::Error::from)
            },
        )
        .await
        {
            Ok(result) => return Ok(result),
            Err(error) => {
                let error = receive_protocol_failure(
                    error,
                    TransferPhase::Metadata,
                    "collection metadata transfer was interrupted",
                );
                tracing::error!(attempt, "collection metadata attempt failed");
                last_err = Some(error);
                if attempt < retry_policy.size_fetch_retry_limit {
                    tokio::time::sleep(size_fetch_backoff(attempt, retry_policy)).await;
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        receive_failure(
            anyhow::anyhow!("unknown error getting sizes"),
            TransferErrorCode::Internal,
            TransferPhase::Metadata,
            false,
            "unable to fetch collection metadata",
        )
    }))
}

/// Calculate the transfer-byte ceiling that includes collection protocol overhead.
fn max_transfer_limit(plan: &DownloadPlan, limits: ReceiveLimits) -> Option<u64> {
    limits.max_total_size_bytes.map(|limit| {
        limit
            .get()
            .saturating_add(plan.transfer_total.saturating_sub(plan.payload_size))
    })
}

/// Consume one get stream without an additional byte ceiling.
#[cfg(test)]
async fn process_get_stream_with_reporter<S>(
    stream: &mut S,
    already_downloaded: u64,
    reporter: &mut ReceiverProgressReporter,
    idle_timeout: Option<Duration>,
) -> anyhow::Result<()>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    process_get_stream_with_limit(
        stream,
        already_downloaded,
        reporter,
        idle_timeout,
        None,
        None,
    )
    .await
}

/// Consume one get stream and reject progress beyond the configured byte ceiling.
async fn process_get_stream_with_limit<S>(
    stream: &mut S,
    already_downloaded: u64,
    reporter: &mut ReceiverProgressReporter,
    idle_timeout: Option<Duration>,
    max_transfer_bytes: Option<u64>,
    cache_lease: Option<&ReceiveCacheLease>,
) -> anyhow::Result<()>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    let mut seen_done = false;
    while let Some(item) = next_get_progress_item(stream, idle_timeout).await? {
        match item {
            GetProgressItem::Progress(offset) => {
                tracing::trace!(offset, "received download progress");
                let downloaded = already_downloaded.saturating_add(offset);
                if let Some(max_transfer_bytes) = max_transfer_bytes {
                    anyhow::ensure!(
                        downloaded <= max_transfer_bytes,
                        "received transfer exceeds the configured byte limit"
                    );
                }
                if let Some(cache_lease) = cache_lease {
                    cache_lease.check_capacity()?;
                }
                reporter.on_progress(downloaded);
            }
            GetProgressItem::Done(value) => {
                let _stats = value;
                if let Some(cache_lease) = cache_lease {
                    cache_lease.check_capacity()?;
                }
                reporter.emit_completed_progress();
                seen_done = true;
                break;
            }
            GetProgressItem::Error(cause) => {
                let error = show_get_error(cause);
                anyhow::bail!(error);
            }
        }
    }
    anyhow::ensure!(seen_done, "download stream ended before completion");
    Ok(())
}

/// Wait for the next stream update, restarting the idle timer after each update.
async fn next_get_progress_item<S>(
    stream: &mut S,
    idle_timeout: Option<Duration>,
) -> anyhow::Result<Option<GetProgressItem>>
where
    S: n0_future::Stream<Item = GetProgressItem> + Unpin + Send,
{
    match idle_timeout {
        Some(timeout) => tokio::time::timeout(timeout, stream.next())
            .await
            .map_err(|_| {
                receive_failure(
                    anyhow::anyhow!(
                        "download stream timed out after {} ms without progress",
                        timeout.as_millis()
                    ),
                    TransferErrorCode::Timeout,
                    TransferPhase::Transferring,
                    true,
                    "download stalled without progress",
                )
            }),
        None => Ok(stream.next().await),
    }
}
