/// 只检查一次 Ctrl+C future，避免准备步骤在取消后继续打开资源。
async fn ctrl_c_requested<F>(signal: Pin<&mut F>) -> bool
where
    F: Future<Output = std::io::Result<()>> + ?Sized,
{
    tokio::select! {
        biased;
        _ = signal => true,
        _ = tokio::task::yield_now() => false,
    }
}

/// Prepare endpoint with the given options
async fn prepare_endpoint(options: &SendOptions) -> anyhow::Result<Endpoint> {
    let mut builder = base_endpoint_builder(options, vec![iroh_blobs::protocol::ALPN.to_vec()])?;

    if options.ticket_type == AddrInfoOptions::Id {
        builder = builder.address_lookup(PkarrPublisher::n0_dns());
    }

    builder.bind().await.map_err(Into::into)
}

/// Prepare temporary directory for blob storage
fn prepare_temp_directory() -> anyhow::Result<PathBuf> {
    unique_temp_dir(".sendmer-send-")
}

/// Validate the path to be shared
fn validate_share_path(path: &Path, allow_empty_directories: bool) -> anyhow::Result<()> {
    let canonical_cwd = std::env::current_dir()?.canonicalize()?;
    let source_metadata = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "share path {} does not exist or cannot be accessed",
            path.display()
        )
    })?;
    anyhow::ensure!(
        !source_metadata.file_type().is_symlink(),
        "cannot share symbolic link {}; symbolic links are not supported",
        path.display()
    );
    let canonical_path = path.canonicalize().with_context(|| {
        format!(
            "share path {} does not exist or cannot be accessed",
            path.display()
        )
    })?;

    if canonical_path == canonical_cwd {
        anyhow::bail!("can not share from the current directory");
    }

    anyhow::ensure!(
        canonical_path.is_file() || canonical_path.is_dir(),
        "share path {} is not a file or directory",
        path.display()
    );

    if canonical_path.is_dir() && !allow_empty_directories {
        validate_share_directory_contents(&canonical_path)?;
    }

    Ok(())
}

/// Reject source entries that the current file-only collection format cannot preserve.
///
/// Each directory must contain a regular file somewhere below it. This prevents an
/// empty directory or a symbolic link from being silently omitted during import.
fn validate_share_directory_contents(root: &Path) -> anyhow::Result<()> {
    let mut directories = BTreeSet::new();
    let mut directories_with_files = HashSet::new();

    for entry in WalkDir::new(root) {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type().is_dir() {
            directories.insert(path.to_path_buf());
            continue;
        }
        if entry.file_type().is_file() {
            for ancestor in path.ancestors().skip(1) {
                if !ancestor.starts_with(root) {
                    break;
                }
                directories_with_files.insert(ancestor.to_path_buf());
            }
            continue;
        }
        if entry.file_type().is_symlink() {
            anyhow::bail!(
                "cannot share symbolic link {}; symbolic links are not supported",
                path.display()
            );
        }
        anyhow::bail!("cannot share unsupported source entry {}", path.display());
    }

    if let Some(empty_directory) = directories
        .iter()
        .find(|directory| !directories_with_files.contains(*directory))
    {
        anyhow::bail!(
            "cannot share empty directory {}; empty directories are not supported",
            empty_directory.display()
        );
    }

    Ok(())
}

/// Enforce optional share-size limits before endpoint or temp-store setup.
fn validate_share_limits(
    path: &Path,
    max_files: Option<NonZeroU64>,
    max_total_size_bytes: Option<NonZeroU64>,
) -> anyhow::Result<()> {
    if max_files.is_none() && max_total_size_bytes.is_none() {
        return Ok(());
    }

    let mut file_count = 0_u64;
    let mut total_size_bytes = 0_u64;
    for entry in WalkDir::new(path) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }

        if let Some(max_files) = max_files {
            file_count = file_count.saturating_add(1);
            if file_count > max_files.get() {
                anyhow::bail!(
                    "share contains more than {} files; --max-files is {}",
                    max_files,
                    max_files
                );
            }
        }

        if let Some(max_total_size_bytes) = max_total_size_bytes {
            let file_size = entry.metadata()?.len();
            total_size_bytes = total_size_bytes
                .checked_add(file_size)
                .ok_or_else(|| anyhow::anyhow!("share total size exceeds u64 range"))?;
            if total_size_bytes > max_total_size_bytes.get() {
                anyhow::bail!(
                    "share contains more than {} bytes; --max-total-size is {}",
                    max_total_size_bytes,
                    max_total_size_bytes
                );
            }
        }
    }

    Ok(())
}

/// Recheck limits against the bytes and files that the blob store actually imported.
///
/// The first metadata scan happens before storage setup. A source file can change between
/// that scan and import, so the imported blob totals are the final limit decision.
fn validate_imported_share_limits(
    imported: &ImportedCollection,
    max_files: Option<NonZeroU64>,
    max_total_size_bytes: Option<NonZeroU64>,
) -> anyhow::Result<()> {
    validate_imported_share_totals(
        imported.file_count,
        imported.size,
        max_files,
        max_total_size_bytes,
    )
}

fn validate_imported_share_totals(
    file_count: u64,
    total_size_bytes: u64,
    max_files: Option<NonZeroU64>,
    max_total_size_bytes: Option<NonZeroU64>,
) -> anyhow::Result<()> {
    if let Some(max_files) = max_files {
        anyhow::ensure!(
            file_count <= max_files.get(),
            "imported share contains more than {} files; --max-files is {}",
            max_files,
            max_files
        );
    }
    if let Some(max_total_size_bytes) = max_total_size_bytes {
        anyhow::ensure!(
            total_size_bytes <= max_total_size_bytes.get(),
            "imported share contains more than {} bytes; --max-total-size is {}",
            max_total_size_bytes,
            max_total_size_bytes
        );
    }
    Ok(())
}
