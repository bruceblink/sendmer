use super::context::{ReceiveArtifacts, ReceiveContext};
use super::errors::receive_failure;
use super::limits::validate_receive_limits;
use super::retry::download_missing_data;
use crate::core::events::{TransferErrorCode, TransferPhase};
use crate::core::filesystem::{
    collection_root_name, export_atomically, export_manifest_atomically,
};
use crate::core::manifest::{MANIFEST_COLLECTION_NAME, ManifestEntryKind, TransferManifest};
use crate::core::options::ReceiveLimits;
use crate::core::progress::TransferEventEmitter;
use anyhow::Context;
use iroh_blobs::api::Store;
use iroh_blobs::api::proto::BlobStatus;
use iroh_blobs::format::collection::Collection;
use std::path::{Path, PathBuf};

/// 读取本地 collection 中每个 payload 的已验证大小。
async fn collection_payload_totals(
    db: &Store,
    collection: &Collection,
    limits: ReceiveLimits,
) -> anyhow::Result<(u64, u64)> {
    if let Some(max_files) = limits.max_files {
        anyhow::ensure!(
            collection.len() as u64 <= max_files.get(),
            "received collection contains more than the local file limit of {}",
            max_files
        );
    }
    let mut total_size = 0_u64;
    for (_name, hash) in collection.iter() {
        let status = db.status(*hash).await.map_err(anyhow::Error::from)?;
        let size = match status {
            BlobStatus::Complete { size } => size,
            BlobStatus::Partial { .. } => {
                anyhow::bail!("received collection payload {hash} is incomplete")
            }
            BlobStatus::NotFound => anyhow::bail!("received collection payload {hash} is missing"),
        };
        total_size = total_size
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("received collection payload size exceeds u64 range"))?;
        if let Some(max_total_size_bytes) = limits.max_total_size_bytes {
            anyhow::ensure!(
                total_size <= max_total_size_bytes.get(),
                "received collection exceeds the local byte limit of {}",
                max_total_size_bytes
            );
        }
    }
    Ok((collection.len() as u64, total_size))
}

pub(super) async fn receive_once(
    context: &mut ReceiveContext,
    output_dir: &Path,
    event_emitter: TransferEventEmitter,
) -> anyhow::Result<ReceiveArtifacts> {
    tracing::trace!("load done!");
    download_missing_data(context, event_emitter.clone()).await?;
    let collection = context
        .load_collection()
        .await
        .context("load received collection")?;
    let manifest_hash = collection
        .iter()
        .find_map(|(name, hash)| (name == MANIFEST_COLLECTION_NAME).then_some(*hash));
    match manifest_hash {
        Some(hash) => receive_manifest(context, output_dir, event_emitter, collection, hash).await,
        None => receive_legacy_collection(context, output_dir, event_emitter, collection).await,
    }
}

async fn receive_manifest(
    context: &mut ReceiveContext,
    output_dir: &Path,
    event_emitter: TransferEventEmitter,
    collection: Collection,
    manifest_hash: iroh_blobs::Hash,
) -> anyhow::Result<ReceiveArtifacts> {
    let manifest_bytes = context.db.get_bytes(manifest_hash).await.map_err(|error| {
        receive_failure(
            error.into(),
            TransferErrorCode::TransferInterrupted,
            TransferPhase::Metadata,
            false,
            "received transfer manifest is unavailable",
        )
    })?;
    let manifest = TransferManifest::from_json_bytes(&manifest_bytes).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::RemoteRejected,
            TransferPhase::Metadata,
            false,
            "received transfer manifest is invalid",
        )
    })?;
    let file_names = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == ManifestEntryKind::File)
        .map(|entry| entry.path.components.join("/"))
        .collect::<Vec<_>>();
    if !file_names.is_empty() {
        event_emitter.emit_file_names(file_names);
    }
    let root_item_path = output_dir.join(manifest.root.path.to_path_buf()?);
    let file_count = manifest
        .entries
        .iter()
        .filter(|entry| entry.kind == ManifestEntryKind::File)
        .count() as u64;
    let payload_size = manifest.total_payload_size().map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::RemoteRejected,
            TransferPhase::Metadata,
            false,
            "received transfer manifest payload sizes are invalid",
        )
    })?;
    validate_receive_limits(file_count, payload_size, context.limits).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::RemoteRejected,
            TransferPhase::Metadata,
            false,
            "received transfer exceeds local resource limits",
        )
    })?;
    export_manifest_atomically(
        &context.db,
        &collection,
        &manifest,
        output_dir,
        &mut context.active_staging_dir,
    )
    .await
    .context("export transfer manifest")
    .map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Exporting,
            false,
            "unable to export transfer manifest",
        )
    })?;
    Ok(ReceiveArtifacts {
        total_files: file_count,
        payload_size,
        root_item_path,
    })
}

async fn receive_legacy_collection(
    context: &mut ReceiveContext,
    output_dir: &Path,
    event_emitter: TransferEventEmitter,
    collection: Collection,
) -> anyhow::Result<ReceiveArtifacts> {
    emit_collection_file_names(&event_emitter, &collection);
    let root_item_path = resolve_root_item_path(output_dir, &collection)
        .context("resolve received output path")
        .map_err(|error| {
            receive_failure(
                error,
                TransferErrorCode::RemoteRejected,
                TransferPhase::Metadata,
                false,
                "received collection contains unsupported paths",
            )
        })?;
    let (total_files, payload_size) =
        collection_payload_totals(&context.db, &collection, context.limits)
            .await
            .context("inspect received collection payloads")
            .map_err(|error| {
                receive_failure(
                    error,
                    TransferErrorCode::Filesystem,
                    TransferPhase::Metadata,
                    false,
                    "unable to inspect received file sizes",
                )
            })?;
    validate_receive_limits(total_files, payload_size, context.limits).map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::RemoteRejected,
            TransferPhase::Metadata,
            false,
            "received transfer exceeds local resource limits",
        )
    })?;
    export_atomically(
        &context.db,
        collection,
        output_dir,
        &mut context.active_staging_dir,
    )
    .await
    .context("export received files")
    .map_err(|error| {
        receive_failure(
            error,
            TransferErrorCode::Filesystem,
            TransferPhase::Exporting,
            false,
            "unable to export received files",
        )
    })?;
    Ok(ReceiveArtifacts {
        total_files,
        payload_size,
        root_item_path,
    })
}

fn emit_collection_file_names(emitter: &TransferEventEmitter, collection: &Collection) {
    let file_names = collect_file_names(collection);
    if !file_names.is_empty() {
        emitter.emit_file_names(file_names);
    }
}

fn collect_file_names(collection: &Collection) -> Vec<String> {
    collection
        .iter()
        .map(|(name, _hash)| name.to_string())
        .collect()
}

fn resolve_root_item_path(output_dir: &Path, collection: &Collection) -> anyhow::Result<PathBuf> {
    let root = collection_root_name(collection)?;
    Ok(output_dir.join(root))
}
