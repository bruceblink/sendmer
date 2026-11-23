pub fn resolve_output_dir(output_dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match output_dir {
        Some(path) if path.is_absolute() => Ok(path),
        // FsStore requires absolute export targets. Resolve before receiving without
        // requiring the destination to exist or changing its symlink semantics.
        Some(path) => Ok(std::path::absolute(path)?),
        None => Ok(std::env::current_dir()?),
    }
}

/// 根据集合内的名称生成导出路径，同时验证每个路径组件的合法性。
pub fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    if root.exists() {
        anyhow::ensure!(
            root.is_dir(),
            "output root {} is not a directory",
            root.display()
        );
    }
    std::fs::create_dir_all(root)?;
    let canonical_root = root.canonicalize()?;
    let parts = name.split('/');
    let mut path = root.to_path_buf();
    for part in parts {
        validate_path_component(part)?;
        path.push(part);
    }

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid export target"))?;
    let canonical_existing_parent = canonicalize_existing_parent(parent)?;
    anyhow::ensure!(
        canonical_existing_parent.starts_with(&canonical_root),
        "final path must be within the root directory"
    );
    std::fs::create_dir_all(parent)?;

    let canonical_parent = parent.canonicalize()?;
    anyhow::ensure!(
        canonical_parent.starts_with(&canonical_root),
        "final path must be within the root directory"
    );

    ensure_export_target_is_absent(&path)?;

    Ok(path)
}

/// Refuse an existing staging target before the blob store writes to it.
///
/// `symlink_metadata` sees dangling symlinks and, on case-insensitive file systems,
/// also catches differently cased names that would otherwise overwrite an earlier entry.
pub fn ensure_export_target_is_absent(path: &Path) -> anyhow::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            anyhow::bail!("export target must not be a symbolic link")
        }
        Ok(_) => anyhow::bail!("export target {} already exists", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Resolve the nearest existing ancestor so symlink escapes are rejected before
/// creating any missing export directories outside the configured root.
pub fn canonicalize_existing_parent(path: &Path) -> anyhow::Result<PathBuf> {
    let mut current = path;
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(_) => return Ok(current.canonicalize()?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                current = current
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("invalid export target"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// 验证单个路径组件是否合法（不应包含分隔符 `/`）。
pub fn validate_path_component(component: &str) -> anyhow::Result<()> {
    // Check for empty components
    anyhow::ensure!(!component.is_empty(), "path component cannot be empty");

    // Check for path separators
    anyhow::ensure!(
        !component.contains('/') && !component.contains('\\'),
        "path components must not contain path separators"
    );

    // Check for path traversal attempts
    anyhow::ensure!(component != "..", "path traversal not allowed: '..'");
    anyhow::ensure!(component != ".", "relative path not allowed: '.'");

    // Check for absolute paths
    anyhow::ensure!(
        !component.starts_with('/'),
        "absolute path components not allowed"
    );

    validate_windows_utf8_component(component).map_err(|error| {
        anyhow::anyhow!("legacy collection path component is not portable: {error}")
    })?;

    // Optional: Check for hidden files (starting with '.')
    // Uncomment if you want to restrict hidden files
    // anyhow::ensure!(
    //     !component.starts_with('.') || component.len() == 1,
    //     "hidden files not allowed"
    // );

    Ok(())
}

fn filesystem_failure(
    error: anyhow::Error,
    code: TransferErrorCode,
    phase: TransferPhase,
    retryable: bool,
    message: &'static str,
) -> anyhow::Error {
    classify_transfer_error(error, TransferError::new(code, phase, retryable, message))
}
