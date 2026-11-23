struct ReceiveContext {
    ticket: BlobTicket,
    addr: iroh::EndpointAddr,
    endpoint: Endpoint,
    iroh_data_dir: PathBuf,
    db: Store,
    retry_policy: ReceiveRetryPolicy,
    limits: ReceiveLimits,
    cache_lease: Option<ReceiveCacheLease>,
    active_staging_dir: Option<PathBuf>,
}

struct ReceiveArtifacts {
    total_files: u64,
    payload_size: u64,
    root_item_path: PathBuf,
}

#[derive(Debug)]
struct DownloadPlan {
    total_files: u64,
    payload_size: u64,
    transfer_total: u64,
}

impl ReceiveContext {
    async fn prepare(
        ticket: BlobTicket,
        options: &ReceiveOptions,
        limits: ReceiveLimits,
    ) -> anyhow::Result<Self> {
        options.retry_policy.validate().map_err(|error| {
            receive_failure(
                error,
                TransferErrorCode::InvalidInput,
                TransferPhase::Preparing,
                false,
                "invalid receive retry policy",
            )
        })?;
        if let Some(cache) = &options.receive_cache {
            cache.validate().map_err(|error| {
                receive_failure(
                    error,
                    TransferErrorCode::InvalidInput,
                    TransferPhase::Preparing,
                    false,
                    "invalid receive cache options",
                )
            })?;
        }
        let addr = ticket.addr().clone();
        let (endpoint, iroh_data_dir, db, cache_lease) = prepare_env(&ticket, options).await?;
        Ok(Self {
            ticket,
            addr,
            endpoint,
            iroh_data_dir,
            db,
            retry_policy: options.retry_policy,
            limits,
            cache_lease,
            active_staging_dir: None,
        })
    }

    fn hash_and_format(&self) -> iroh_blobs::HashAndFormat {
        self.ticket.hash_and_format()
    }

    async fn load_collection(&self) -> anyhow::Result<Collection> {
        Collection::load(self.hash_and_format().hash, &self.db)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))
            .map_err(|error| {
                receive_failure(
                    error,
                    TransferErrorCode::TransferInterrupted,
                    TransferPhase::Metadata,
                    false,
                    "received collection metadata is unavailable",
                )
            })
    }
}

impl DownloadPlan {
    /// Build transfer totals from a collection hash sequence and its child sizes.
    ///
    /// The first child is collection metadata, so it is excluded from the user-facing
    /// file count and payload size. The root hash sequence itself still travels over
    /// the network and is included in progress accounting.
    fn from_hash_seq_and_sizes(
        hash_seq: &iroh_blobs::hashseq::HashSeq,
        sizes: &[u64],
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !hash_seq.is_empty() && !sizes.is_empty(),
            "collection metadata must contain at least one child"
        );
        anyhow::ensure!(
            hash_seq.len() == sizes.len(),
            "collection metadata child count does not match its hash sequence"
        );
        anyhow::ensure!(
            sizes.len() <= MAX_LEGACY_COLLECTION_ITEMS.saturating_add(1),
            "collection metadata contains too many children (maximum {MAX_LEGACY_COLLECTION_ITEMS})"
        );
        let total_files = u64::try_from(sizes.len() - 1)
            .map_err(|_| anyhow::anyhow!("collection file count exceeds u64 range"))?;
        let payload_size = sizes.iter().skip(1).try_fold(0_u64, |total, size| {
            total
                .checked_add(*size)
                .ok_or_else(|| anyhow::anyhow!("collection payload size exceeds u64 range"))
        })?;
        let children_size = sizes.iter().try_fold(0_u64, |total, size| {
            total
                .checked_add(*size)
                .ok_or_else(|| anyhow::anyhow!("collection metadata size exceeds u64 range"))
        })?;
        let hash_sequence_size = u64::try_from(hash_seq.len())
            .map_err(|_| anyhow::anyhow!("collection hash sequence length exceeds u64 range"))?
            .checked_mul(32)
            .ok_or_else(|| anyhow::anyhow!("collection hash sequence size exceeds u64 range"))?;
        let transfer_total = hash_sequence_size
            .checked_add(children_size)
            .ok_or_else(|| anyhow::anyhow!("collection transfer size exceeds u64 range"))?;
        Ok(Self {
            total_files,
            payload_size,
            transfer_total,
        })
    }
}
