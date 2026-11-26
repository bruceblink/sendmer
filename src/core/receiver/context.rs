use super::RECEIVE_TEMP_DIR_PREFIX;
use super::errors::receive_failure;
use super::shutdown::{close_receive_endpoint, remove_temp_receive_dir};
use crate::core::events::{TransferErrorCode, TransferPhase, TransferSessionId};
use crate::core::options::{
    MAX_LEGACY_COLLECTION_ITEMS, ReceiveLimits, ReceiveOptions, ReceiveRetryPolicy,
};
use crate::core::receive_cache::ReceiveCacheLease;
use crate::core::storage::{load_fs_store, unique_temp_dir};
use crate::core::transport::base_endpoint_builder;
use iroh::Endpoint;
use iroh::address_lookup::DnsAddressLookup;
use iroh_blobs::api::Store;
use iroh_blobs::format::collection::Collection;
use iroh_blobs::ticket::BlobTicket;
use std::path::PathBuf;

pub(super) struct ReceiveContext {
    pub(super) ticket: BlobTicket,
    pub(super) addr: iroh::EndpointAddr,
    pub(super) endpoint: Endpoint,
    pub(super) iroh_data_dir: PathBuf,
    pub(super) db: Store,
    pub(super) retry_policy: ReceiveRetryPolicy,
    pub(super) limits: ReceiveLimits,
    pub(super) cache_lease: Option<ReceiveCacheLease>,
    pub(super) active_staging_dir: Option<PathBuf>,
}

pub(super) struct ReceiveArtifacts {
    pub(super) total_files: u64,
    pub(super) payload_size: u64,
    pub(super) root_item_path: PathBuf,
}

#[derive(Debug)]
pub(super) struct DownloadPlan {
    pub(super) total_files: u64,
    pub(super) payload_size: u64,
    pub(super) transfer_total: u64,
}

impl ReceiveContext {
    pub(super) async fn prepare(
        ticket: BlobTicket,
        options: &ReceiveOptions,
        limits: ReceiveLimits,
        session_id: TransferSessionId,
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
        if let Some(lease) = &cache_lease {
            lease.begin_recovery(session_id);
        }
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

    pub(super) fn hash_and_format(&self) -> iroh_blobs::HashAndFormat {
        self.ticket.hash_and_format()
    }

    pub(super) async fn load_collection(&self) -> anyhow::Result<Collection> {
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
    pub(super) fn from_hash_seq_and_sizes(
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
