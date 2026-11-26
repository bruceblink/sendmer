//! Optional TM1 source import.

use super::{
    ImportedCollection, ImportedSource, SourceFingerprint, import_parallelism, import_sources,
};
use crate::core::manifest::MANIFEST_COLLECTION_NAME;
use crate::core::manifest::{
    MANIFEST_ENTRY_PREFIX, ManifestEntry, ManifestEntryKind, ManifestMetadata, ManifestPath,
    ManifestPayload, ManifestPermissions, ManifestRoot, TransferManifest, UnixTimestamp,
};
use anyhow::Context;
use iroh_blobs::{api::Store, format::collection::Collection};
use std::{num::NonZeroU64, path::PathBuf};
use walkdir::WalkDir;

/// Collect every logical directory and file for TM1 while preserving the source platform name.
fn collect_manifest_sources(path: PathBuf) -> anyhow::Result<(ManifestRoot, Vec<ManifestSource>)> {
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root_parent = path.parent().context("manifest source has no parent")?;
    let mut raw_entries = Vec::new();
    for entry in WalkDir::new(&path) {
        let entry = entry?;
        let source_path = entry.path().to_path_buf();
        let metadata = std::fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "cannot share symbolic link {}; symbolic links are not supported",
                source_path.display()
            );
        }
        let kind = if metadata.is_dir() {
            ManifestEntryKind::Directory
        } else if metadata.is_file() {
            ManifestEntryKind::File
        } else {
            anyhow::bail!(
                "cannot share unsupported source entry {}",
                source_path.display()
            );
        };
        let relative = source_path.strip_prefix(root_parent)?.to_path_buf();
        raw_entries.push((source_path, relative, kind, metadata));
    }

    let encoding =
        ManifestPath::encoding_for_paths(raw_entries.iter().map(|(_, relative, _, _)| relative));
    let mut sources = raw_entries
        .into_iter()
        .map(|(source_path, relative, kind, metadata)| {
            let relative = ManifestPath::from_path_with_encoding(&relative, true, encoding)?;
            let size = if kind == ManifestEntryKind::File {
                metadata.len()
            } else {
                0
            };
            Ok(ManifestSource {
                path: source_path,
                relative,
                kind,
                size,
                metadata: manifest_metadata(&metadata),
                fingerprint: if kind == ManifestEntryKind::File {
                    Some(SourceFingerprint::from_metadata(&metadata)?)
                } else {
                    None
                },
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    sources.sort_by(|left, right| left.relative.cmp(&right.relative));

    let root_source = sources
        .first()
        .ok_or_else(|| anyhow::anyhow!("manifest source is empty"))?;
    let root = ManifestRoot {
        kind: root_source.kind,
        path: root_source.relative.clone(),
    };
    if root.kind == ManifestEntryKind::File {
        anyhow::ensure!(
            sources.len() == 1,
            "file manifest contains unexpected entries"
        );
    }
    Ok((root, sources))
}

/// Convert filesystem metadata into the platform-safe subset carried by TM1.
fn manifest_metadata(metadata: &std::fs::Metadata) -> ManifestMetadata {
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        Some(ManifestPermissions {
            posix_mode: Some(metadata.permissions().mode() & 0o7777),
            windows_read_only: None,
        })
    };
    #[cfg(windows)]
    let permissions = Some(ManifestPermissions {
        posix_mode: None,
        windows_read_only: Some(metadata.permissions().readonly()),
    });
    #[cfg(not(any(unix, windows)))]
    let permissions = None;

    ManifestMetadata {
        permissions,
        modified: metadata.modified().ok().and_then(unix_timestamp),
    }
}

fn unix_timestamp(time: std::time::SystemTime) -> Option<UnixTimestamp> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => Some(UnixTimestamp {
            seconds: i64::try_from(duration.as_secs()).ok()?,
            nanos: duration.subsec_nanos(),
        }),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs()).ok()?;
            if duration.subsec_nanos() == 0 {
                Some(UnixTimestamp {
                    seconds: -seconds,
                    nanos: 0,
                })
            } else {
                Some(UnixTimestamp {
                    seconds: -seconds - 1,
                    nanos: 1_000_000_000 - duration.subsec_nanos(),
                })
            }
        }
    }
}

/// Import payloads and build a reserved-entry Collection containing the TM1 JSON blob.
pub(super) async fn import_manifest(
    path: PathBuf,
    db: &Store,
    max_import_memory_bytes: Option<NonZeroU64>,
) -> anyhow::Result<ImportedCollection> {
    let (root, sources) = collect_manifest_sources(path)?;
    let file_sources = sources
        .iter()
        .filter(|source| source.kind == ManifestEntryKind::File)
        .map(|source| {
            let fingerprint = source
                .fingerprint
                .clone()
                .ok_or_else(|| anyhow::anyhow!("manifest file source is missing metadata"))?;
            Ok(ImportedSource {
                name: source.relative.components.join("/"),
                path: source.path.clone(),
                size: source.size,
                fingerprint,
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let imported = import_sources(
        db,
        file_sources,
        import_parallelism(sources.len()),
        max_import_memory_bytes,
    )
    .await?;
    let mut imported_by_name = imported
        .into_iter()
        .map(|item| (item.name.clone(), item))
        .collect::<std::collections::BTreeMap<_, _>>();

    let entries = sources
        .iter()
        .filter(|source| {
            !(source.kind == ManifestEntryKind::Directory && source.relative == root.path)
        })
        .enumerate()
        .map(|(index, source)| {
            let id = format!("{index:08x}");
            let payload = if source.kind == ManifestEntryKind::File {
                let key = source.relative.components.join("/");
                let imported = imported_by_name
                    .get(&key)
                    .ok_or_else(|| anyhow::anyhow!("missing imported manifest payload {key}"))?;
                Some(ManifestPayload {
                    collection_name: format!("{MANIFEST_ENTRY_PREFIX}{id}"),
                    size: imported.size,
                })
            } else {
                None
            };
            Ok(ManifestEntry {
                id,
                kind: source.kind,
                path: source.relative.clone(),
                payload,
                metadata: source.metadata.clone(),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let manifest = TransferManifest {
        schema_version: crate::core::manifest::MANIFEST_SCHEMA_VERSION,
        root,
        entries,
    };
    let manifest_bytes = manifest.to_json_bytes()?;
    let manifest_tag = db.add_bytes(manifest_bytes).temp_tag().await?;

    let mut collection = Collection::default();
    collection.push(MANIFEST_COLLECTION_NAME.to_owned(), manifest_tag.hash());
    let mut tags = vec![manifest_tag];
    let file_count = u64::try_from(imported_by_name.len())
        .map_err(|_| anyhow::anyhow!("manifest file count exceeds u64 range"))?;
    for (index, entry) in manifest.entries.iter().enumerate() {
        if entry.kind != ManifestEntryKind::File {
            continue;
        }
        let key = entry.path.components.join("/");
        let imported = imported_by_name
            .remove(&key)
            .ok_or_else(|| anyhow::anyhow!("missing imported manifest payload {key}"))?;
        collection.push(
            format!("{MANIFEST_ENTRY_PREFIX}{index:08x}"),
            imported.temp_tag.hash(),
        );
        tags.push(imported.temp_tag);
    }
    let size = manifest
        .entries
        .iter()
        .filter_map(|entry| entry.payload.as_ref().map(|payload| payload.size))
        .try_fold(0_u64, |total, payload_size| {
            total
                .checked_add(payload_size)
                .ok_or_else(|| anyhow::anyhow!("imported manifest payload size exceeds u64 range"))
        })?;
    let temp_tag = collection.clone().store(db).await?;
    drop(tags);
    Ok(ImportedCollection {
        temp_tag,
        file_count,
        size,
        _collection: collection,
    })
}

struct ManifestSource {
    path: PathBuf,
    relative: ManifestPath,
    kind: ManifestEntryKind,
    size: u64,
    metadata: ManifestMetadata,
    fingerprint: Option<SourceFingerprint>,
}
