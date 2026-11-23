/// 将集合中的各个 blob 导出到 `output_dir`。
///
/// 该函数会为每个条目创建目标路径并通过 `db.export_with_opts` 执行导出流。
/// Export a collection into staging, then commit complete top-level roots.
///
/// Final paths are untouched until every blob reports `Done`; a failed export only
/// removes the staging directory and therefore cannot leave a partial user file.
pub async fn export_atomically(
    db: &Store,
    collection: Collection,
    output_dir: &Path,
    active_staging_dir: &mut Option<PathBuf>,
) -> anyhow::Result<()> {
    let root = collection_root_name(&collection)?;
    ensure_export_root_available(output_dir, &root)?;
    let staging_dir = create_staging_dir(output_dir)?;
    *active_staging_dir = Some(staging_dir.clone());

    if let Err(error) = export_to_staging(db, &collection, &staging_dir).await {
        cleanup_staging_dir(&staging_dir);
        return Err(error);
    }

    if let Err(error) = commit_staged_export(&staging_dir, output_dir, &root) {
        cleanup_staging_dir(&staging_dir);
        return Err(error);
    }

    Ok(())
}

/// Export a TM1 directory tree after validating all logical paths and payload mappings.
pub async fn export_manifest_atomically(
    db: &Store,
    collection: &Collection,
    manifest: &TransferManifest,
    output_dir: &Path,
    active_staging_dir: &mut Option<PathBuf>,
) -> anyhow::Result<PathBuf> {
    let root_path = manifest.root.path.to_path_buf()?;
    anyhow::ensure!(
        root_path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_))),
        "manifest root must be relative"
    );
    ensure_export_root_available_path(output_dir, &root_path)?;
    validate_manifest_export_targets(manifest)?;
    let staging_dir = create_staging_dir(output_dir)?;
    *active_staging_dir = Some(staging_dir.clone());
    let result = async {
        let staged_root = staging_dir.join(&root_path);
        if manifest.root.kind == ManifestEntryKind::Directory {
            std::fs::create_dir_all(&staged_root)?;
        }
        let mut payloads = std::collections::BTreeMap::new();
        for (name, hash) in collection.iter() {
            if name.starts_with(crate::core::manifest::MANIFEST_ENTRY_PREFIX) {
                payloads.insert(name, *hash);
            }
        }
        let mut targets = Vec::new();
        for entry in &manifest.entries {
            let full_path = entry.path.to_path_buf()?;
            anyhow::ensure!(
                full_path
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_))),
                "manifest entry path must contain only normal components"
            );
            let target = staging_dir.join(&full_path);
            anyhow::ensure!(
                target.strip_prefix(&staging_dir).is_ok(),
                "manifest entry escaped staging directory"
            );
            match entry.kind {
                ManifestEntryKind::Directory => {
                    std::fs::create_dir_all(&target)?;
                }
                ManifestEntryKind::File => {
                    let payload = entry
                        .payload
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("manifest file payload is missing"))?;
                    let hash = payloads.get(&payload.collection_name).ok_or_else(|| {
                        anyhow::anyhow!("manifest payload {} is missing", payload.collection_name)
                    })?;
                    let parent = target
                        .parent()
                        .ok_or_else(|| anyhow::anyhow!("manifest file has no parent"))?;
                    std::fs::create_dir_all(parent)?;
                    let mut stream = db
                        .export_with_opts(ExportOptions {
                            hash: *hash,
                            target: target.clone(),
                            mode: ExportMode::Copy,
                        })
                        .stream()
                        .await;
                    process_export_stream(&mut stream, &payload.collection_name).await?;
                    let actual_size = std::fs::metadata(&target)?.len();
                    anyhow::ensure!(
                        actual_size == payload.size,
                        "manifest payload {} size does not match",
                        payload.collection_name
                    );
                }
            }
            targets.push((target, entry));
        }
        for (target, entry) in targets.iter().rev() {
            apply_manifest_metadata(target, &entry.metadata)?;
        }
        commit_staged_export_path(&staging_dir, output_dir, &root_path)?;
        Ok::<PathBuf, anyhow::Error>(output_dir.join(root_path))
    }
    .await;
    if result.is_err() {
        cleanup_staging_dir(&staging_dir);
    }
    result
}

pub fn ensure_export_root_available_path(
    output_dir: &Path,
    root: &Path,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(output_dir)?;
    anyhow::ensure!(output_dir.is_dir(), "output root is not a directory");
    anyhow::ensure!(
        root.components()
            .all(|component| matches!(component, std::path::Component::Normal(_))),
        "manifest root must be relative"
    );
    let target = output_dir.join(root);
    let canonical_output = output_dir.canonicalize()?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("manifest root has no parent"))?;
    let canonical_parent = canonicalize_existing_parent(parent)?;
    anyhow::ensure!(
        canonical_parent.starts_with(&canonical_output),
        "manifest root escapes the output directory"
    );
    match std::fs::symlink_metadata(&target) {
        Ok(_) => Err(target_conflict_error(&target)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn commit_staged_export_path(
    staging_dir: &Path,
    output_dir: &Path,
    root: &Path,
) -> anyhow::Result<()> {
    let staged = staging_dir.join(root);
    let staged_metadata = std::fs::symlink_metadata(&staged)?;
    anyhow::ensure!(
        !staged_metadata.file_type().is_symlink(),
        "staged export root must not be a symbolic link"
    );
    anyhow::ensure!(
        staged_metadata.is_dir(),
        "staged export root is not a directory"
    );
    ensure_export_root_available_path(output_dir, root)?;
    let target = output_dir.join(root);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => return Err(target_conflict_error(&target)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Err(error) = move_staged_root_without_replacing(&staged, &target) {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(target_conflict_error(&target));
        }
        return Err(error.into());
    }
    cleanup_staging_dir(staging_dir);
    Ok(())
}

/// 检查 TM1 在当前主机上的目标路径是否发生大小写碰撞。
pub fn validate_manifest_export_targets(manifest: &TransferManifest) -> anyhow::Result<()> {
    let mut targets = std::collections::BTreeSet::new();
    for entry in &manifest.entries {
        let path = entry.path.to_path_buf()?;
        anyhow::ensure!(
            targets.insert(manifest_target_key(&path)),
            "manifest contains paths that collide on this filesystem"
        );
    }
    Ok(())
}

pub fn manifest_target_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    #[cfg(any(windows, target_os = "macos"))]
    {
        value.to_lowercase()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        value
    }
}

pub fn apply_manifest_metadata(
    target: &Path,
    metadata: &crate::core::manifest::ManifestMetadata,
) -> anyhow::Result<()> {
    // Apply timestamps before the final permission state: a Windows read-only
    // file rejects later metadata writes with ERROR_ACCESS_DENIED.
    if let Some(modified) = metadata.modified {
        filetime::set_file_mtime(
            target,
            filetime::FileTime::from_unix_time(modified.seconds, modified.nanos),
        )?;
    }

    if let Some(permissions) = &metadata.permissions {
        #[cfg(unix)]
        if let Some(mode) = permissions.posix_mode {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(target, std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(windows)]
        if let Some(read_only) = permissions.windows_read_only {
            let mut current = std::fs::metadata(target)?.permissions();
            current.set_readonly(read_only);
            std::fs::set_permissions(target, current)?;
        }
    }
    Ok(())
}

/// Export every collection entry below the staging root without touching final paths.
pub async fn export_to_staging(
    db: &Store,
    collection: &Collection,
    staging_dir: &Path,
) -> anyhow::Result<()> {
    for (name, hash) in collection.iter() {
        let target = get_export_path(staging_dir, name).map_err(|error| {
            filesystem_failure(
                error,
                TransferErrorCode::RemoteRejected,
                TransferPhase::Exporting,
                false,
                "received collection contains invalid paths",
            )
        })?;
        let mut stream = db
            .export_with_opts(ExportOptions {
                hash: *hash,
                target,
                mode: ExportMode::Copy,
            })
            .stream()
            .await;
        process_export_stream(&mut stream, name).await?;
    }
    Ok(())
}

/// Return the only top-level root a collection may export atomically.
///
/// sendmer creates one-root collections for both files and directories.  External
/// multi-root collections are rejected instead of risking a partial commit.
pub fn collection_root_name(collection: &Collection) -> anyhow::Result<String> {
    let mut roots = std::collections::BTreeSet::new();
    for (name, _) in collection.iter() {
        let root = name
            .split('/')
            .next()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| anyhow::anyhow!("collection contains invalid entry name"))?;
        validate_path_component(root)?;
        roots.insert(root.to_owned());
    }

    let Some(root) = roots.pop_first() else {
        anyhow::bail!("collection is empty")
    };
    anyhow::ensure!(
        roots.is_empty(),
        "collection contains multiple top-level roots and cannot be exported atomically"
    );
    Ok(root)
}

pub fn ensure_export_root_available(output_dir: &Path, root: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(output_dir)?;
    anyhow::ensure!(
        output_dir.is_dir(),
        "output root {} is not a directory",
        output_dir.display()
    );

    validate_path_component(root)?;
    let target = output_dir.join(root);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => return Err(target_conflict_error(&target)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    Ok(())
}

/// Create a unique staging directory under the destination so final commit uses rename.
pub fn create_staging_dir(output_dir: &Path) -> anyhow::Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    for attempt in 0..100u32 {
        let candidate = output_dir.join(format!(".sendmer-stage-{stamp}-{attempt}"));
        match std::fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }

    anyhow::bail!("could not create a unique export staging directory")
}

pub fn cleanup_staging_dir(path: &Path) {
    if let Err(error) = std::fs::remove_dir_all(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!("failed to clean export staging directory");
    }
}

/// Commit the complete staging root without replacing a target created concurrently.
pub fn commit_staged_export(
    staging_dir: &Path,
    output_dir: &Path,
    root: &str,
) -> anyhow::Result<()> {
    validate_path_component(root)?;
    let staged = staging_dir.join(root);
    anyhow::ensure!(staged.exists(), "staged export root {root} is missing");
    let target = output_dir.join(root);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => return Err(target_conflict_error(&target)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    if let Err(error) = move_staged_root_without_replacing(&staged, &target) {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            return Err(filesystem_failure(
                error.into(),
                TransferErrorCode::TargetConflict,
                TransferPhase::Exporting,
                false,
                "receive target already exists",
            ));
        }
        return Err(error.into());
    }

    // Moving the root is the commit point. Cleanup can fail after a successful move
    // (for example, because another process created a file in staging), but must not
    // turn a completed export into a retryable failure.
    cleanup_staging_dir(staging_dir);
    Ok(())
}

pub fn target_conflict_error(target: &Path) -> anyhow::Error {
    filesystem_failure(
        anyhow::anyhow!("target {} already exists", target.display()),
        TransferErrorCode::TargetConflict,
        TransferPhase::Exporting,
        false,
        "receive target already exists",
    )
}

/// Move a staged export into place while preserving any target created by another process.
///
/// All release targets have a native no-replace rename primitive.  The fallback
/// retains the preflight check for unsupported platforms, but is not used by releases.
#[cfg(target_os = "linux")]
pub fn move_staged_root_without_replacing(
    source: &Path,
    target: &Path,
) -> std::io::Result<()> {
    let source = path_to_c_string(source)?;
    let target = path_to_c_string(target)?;
    // SAFETY: the paths are valid NUL-terminated strings and AT_FDCWD makes them absolute.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
pub fn move_staged_root_without_replacing(
    source: &Path,
    target: &Path,
) -> std::io::Result<()> {
    let source = path_to_c_string(source)?;
    let target = path_to_c_string(target)?;
    // SAFETY: the paths are valid NUL-terminated strings for renamex_np.
    let result = unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn path_to_c_string(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "export path contains an interior NUL byte",
        )
    })
}

#[cfg(windows)]
pub fn move_staged_root_without_replacing(
    source: &Path,
    target: &Path,
) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both UTF-16 paths are NUL-terminated and no replacement flag is set.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0) } != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn move_staged_root_without_replacing(
    source: &Path,
    target: &Path,
) -> std::io::Result<()> {
    if std::fs::symlink_metadata(target).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("target {} already exists", target.display()),
        ));
    }
    std::fs::rename(source, target)
}

/// 消费一个文件的导出进度流，并且只在收到 `Done` 后报告成功。
///
/// 底层通道提前关闭时，可能已经留下不完整的目标文件；调用方会把该错误纳入
/// 接收失败清理流程，而不会把半导出误报为成功。
pub async fn process_export_stream<S>(stream: &mut S, name: &str) -> anyhow::Result<()>
where
    S: n0_future::Stream<Item = ExportProgressItem> + Unpin + Send,
{
    let mut seen_done = false;
    while let Some(item) = stream.next().await {
        match item {
            ExportProgressItem::Size(_) | ExportProgressItem::CopyProgress(_) => {
                // This library version does not expose export progress to the caller.
            }
            ExportProgressItem::Done => {
                seen_done = true;
                break;
            }
            ExportProgressItem::Error(cause) => {
                anyhow::bail!("error exporting {name}: {cause}");
            }
        }
    }

    anyhow::ensure!(seen_done, "export stream ended before completion");
    Ok(())
}
