/// 将 `path`（文件或目录）导入到给定的 `Store`，并返回导入后的集合信息。
async fn import(
    path: PathBuf,
    db: &Store,
    max_import_memory_bytes: Option<NonZeroU64>,
    manifest_mode: bool,
) -> anyhow::Result<ImportedCollection> {
    if manifest_mode {
        return import_manifest(path, db, max_import_memory_bytes).await;
    }
    let sources = collect_import_sources(path)?;
    let parallelism = import_parallelism(sources.len());
    let imported = import_sources(db, sources, parallelism, max_import_memory_bytes).await?;
    build_collection_from_imports(db, imported).await
}

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
async fn import_manifest(
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

fn import_parallelism(source_count: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    available.min(source_count.max(1))
}

/// Bound estimated source-file bytes held by concurrent imports without claiming an RSS limit.
struct ImportMemoryBudget {
    limit_bytes: u64,
    available_bytes: std::sync::Mutex<u64>,
    notify: Notify,
}

struct ImportMemoryPermit {
    budget: std::sync::Arc<ImportMemoryBudget>,
    bytes: u64,
}

impl ImportMemoryBudget {
    fn new(limit: NonZeroU64) -> Self {
        Self {
            limit_bytes: limit.get(),
            available_bytes: std::sync::Mutex::new(limit.get()),
            notify: Notify::new(),
        }
    }

    /// Reserve a source's estimated bytes, allowing one source larger than the budget to run.
    async fn acquire(self: &std::sync::Arc<Self>, requested_bytes: u64) -> ImportMemoryPermit {
        let bytes = requested_bytes.min(self.limit_bytes);
        loop {
            let notified = self.notify.notified();
            let acquired = {
                let mut available = self
                    .available_bytes
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if *available >= bytes {
                    *available -= bytes;
                    true
                } else {
                    false
                }
            };
            if acquired {
                return ImportMemoryPermit {
                    budget: self.clone(),
                    bytes,
                };
            }
            notified.await;
        }
    }
}

impl Drop for ImportMemoryPermit {
    fn drop(&mut self) {
        let mut available = self
            .budget
            .available_bytes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *available = available
            .saturating_add(self.bytes)
            .min(self.budget.limit_bytes);
        drop(available);
        self.budget.notify.notify_waiters();
    }
}

fn collect_import_sources(path: PathBuf) -> anyhow::Result<Vec<ImportedSource>> {
    let path = path.canonicalize()?;
    anyhow::ensure!(path.exists(), "path {} does not exist", path.display());
    let root = path.parent().context("context get parent")?;

    WalkDir::new(path.clone())
        .into_iter()
        .map(|entry| {
            let entry = entry?;
            if !entry.file_type().is_file() {
                return Ok(None);
            }

            let metadata = entry.metadata()?;
            let size = metadata.len();
            let fingerprint = SourceFingerprint::from_metadata(&metadata)?;
            let path = entry.into_path();
            let relative = path.strip_prefix(root)?;
            let name = canonicalized_path_to_string(relative, true)?;
            validate_legacy_collection_name(&name)?;
            anyhow::Ok(Some(ImportedSource {
                name,
                path,
                size,
                fingerprint,
            }))
        })
        .filter_map(Result::transpose)
        .collect::<anyhow::Result<Vec<_>>>()
}

/// 拒绝旧 Collection 会被接收端识别为 TM1 控制条目的用户文件名。
fn validate_legacy_collection_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        name != MANIFEST_COLLECTION_NAME,
        "legacy collection name is reserved for the transfer manifest"
    );
    Ok(())
}

async fn import_sources(
    db: &Store,
    sources: Vec<ImportedSource>,
    parallelism: usize,
    max_import_memory_bytes: Option<NonZeroU64>,
) -> anyhow::Result<Vec<ImportedBlob>> {
    let budget = max_import_memory_bytes
        .map(ImportMemoryBudget::new)
        .map(std::sync::Arc::new);
    n0_future::stream::iter(sources)
        .map(|source| {
            let db = db.clone();
            let budget = budget.clone();
            async move {
                let permit = match budget {
                    Some(budget) => Some(budget.acquire(source.size).await),
                    None => None,
                };
                let result = import_source(&db, source).await;
                drop(permit);
                result
            }
        })
        .buffered_unordered(parallelism)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()
}

async fn import_source(db: &Store, source: ImportedSource) -> anyhow::Result<ImportedBlob> {
    // 先打开句柄并核对元数据，再把句柄交给 blob store，避免导入期间路径替换。
    let file = tokio::fs::File::open(&source.path)
        .await
        .with_context(|| format!("open source file {}", source.path.display()))?;
    let metadata = file
        .metadata()
        .await
        .with_context(|| format!("inspect source file {}", source.path.display()))?;
    source
        .fingerprint
        .verify_open_metadata(&source.path, &metadata)?;
    let import = db.add_stream(ReaderStream::new(file)).await;
    let mut stream = import.stream().await;
    // add_stream 的实现可能省略 Size 事件，已核对的源文件大小仍是可靠结果。
    let temp_tag = loop {
        let item = stream
            .next()
            .await
            .context("import stream ended without a tag")?;
        match item {
            iroh_blobs::api::blobs::AddProgressItem::Size(size) => {
                anyhow::ensure!(
                    size == source.size,
                    "source file {} changed while importing",
                    source.path.display()
                );
            }
            iroh_blobs::api::blobs::AddProgressItem::CopyProgress(_) => {}
            iroh_blobs::api::blobs::AddProgressItem::CopyDone => {}
            iroh_blobs::api::blobs::AddProgressItem::OutboardProgress(_) => {}
            iroh_blobs::api::blobs::AddProgressItem::Error(cause) => {
                anyhow::bail!("error importing {}: {}", source.name, cause);
            }
            iroh_blobs::api::blobs::AddProgressItem::Done(tt) => {
                break tt;
            }
        }
    };

    Ok(ImportedBlob {
        name: source.name,
        temp_tag,
        size: source.size,
    })
}

async fn build_collection_from_imports(
    db: &Store,
    mut imported: Vec<ImportedBlob>,
) -> anyhow::Result<ImportedCollection> {
    imported.sort_by(|a, b| a.name.cmp(&b.name));
    let file_count = u64::try_from(imported.len())
        .map_err(|_| anyhow::anyhow!("imported file count exceeds u64 range"))?;
    let size = imported.iter().try_fold(0_u64, |total, item| {
        total
            .checked_add(item.size)
            .ok_or_else(|| anyhow::anyhow!("imported file size exceeds u64 range"))
    })?;
    let (collection, tags) = imported
        .into_iter()
        .map(|item| ((item.name, item.temp_tag.hash()), item.temp_tag))
        .unzip::<_, _, Collection, Vec<_>>();
    let temp_tag = collection.clone().store(db).await?;
    drop(tags);
    Ok(ImportedCollection {
        temp_tag,
        file_count,
        size,
        _collection: collection,
    })
}
