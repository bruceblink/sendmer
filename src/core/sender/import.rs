use crate::core::manifest::MANIFEST_COLLECTION_NAME;
#[cfg(feature = "manifest")]
mod manifest_import;
use anyhow::Context;
use iroh_blobs::api::{Store, TempTag};
use iroh_blobs::format::collection::Collection;
#[cfg(feature = "manifest")]
use manifest_import::import_manifest;
use n0_future::{BufferedStreamExt, StreamExt};
use std::num::NonZeroU64;
use std::path::{Component, Path, PathBuf};
use tokio::sync::Notify;
use tokio_util::io::ReaderStream;
use walkdir::WalkDir;

/// 将 `path`（文件或目录）导入到给定的 `Store`，并返回导入后的集合信息。
pub(super) async fn import(
    path: PathBuf,
    db: &Store,
    max_import_memory_bytes: Option<NonZeroU64>,
    manifest_mode: bool,
) -> anyhow::Result<ImportedCollection> {
    if manifest_mode {
        #[cfg(feature = "manifest")]
        return import_manifest(path, db, max_import_memory_bytes).await;
        #[cfg(not(feature = "manifest"))]
        anyhow::bail!("TM1 transfers require the manifest feature");
    }
    let sources = collect_import_sources(path)?;
    let parallelism = import_parallelism(sources.len());
    let imported = import_sources(db, sources, parallelism, max_import_memory_bytes).await?;
    build_collection_from_imports(db, imported).await
}

pub(super) fn import_parallelism(source_count: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1);
    available.min(source_count.max(1))
}

/// Bound estimated source-file bytes held by concurrent imports without claiming an RSS limit.
pub(super) struct ImportMemoryBudget {
    limit_bytes: u64,
    available_bytes: std::sync::Mutex<u64>,
    notify: Notify,
}

pub(super) struct ImportMemoryPermit {
    budget: std::sync::Arc<ImportMemoryBudget>,
    bytes: u64,
}

impl ImportMemoryBudget {
    pub(super) fn new(limit: NonZeroU64) -> Self {
        Self {
            limit_bytes: limit.get(),
            available_bytes: std::sync::Mutex::new(limit.get()),
            notify: Notify::new(),
        }
    }

    /// Reserve a source's estimated bytes, allowing one source larger than the budget to run.
    pub(super) async fn acquire(
        self: &std::sync::Arc<Self>,
        requested_bytes: u64,
    ) -> ImportMemoryPermit {
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

pub(super) fn collect_import_sources(path: PathBuf) -> anyhow::Result<Vec<ImportedSource>> {
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
pub(super) fn validate_legacy_collection_name(name: &str) -> anyhow::Result<()> {
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

pub(super) struct ImportedSource {
    pub(super) name: String,
    path: PathBuf,
    size: u64,
    pub(super) fingerprint: SourceFingerprint,
}

/// 文件导入前记录的元数据，用于确认打开的句柄仍对应扫描到的文件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SourceFingerprint {
    size: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl SourceFingerprint {
    /// 从文件元数据生成指纹；修改时间缺失时无法安全确认源文件身份。
    fn from_metadata(metadata: &std::fs::Metadata) -> anyhow::Result<Self> {
        Ok(Self {
            size: metadata.len(),
            modified: metadata
                .modified()
                .context("source file has no modification timestamp")?,
            #[cfg(unix)]
            device: {
                use std::os::unix::fs::MetadataExt;
                metadata.dev()
            },
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                metadata.ino()
            },
        })
    }

    /// 比较打开句柄的元数据，拒绝扫描后被替换的路径。
    pub(super) fn verify_open_metadata(
        &self,
        path: &Path,
        metadata: &std::fs::Metadata,
    ) -> anyhow::Result<()> {
        let actual = Self::from_metadata(metadata)?;
        anyhow::ensure!(
            self == &actual,
            "source file {} changed between scan and open",
            path.display()
        );
        Ok(())
    }
}

struct ImportedBlob {
    name: String,
    temp_tag: TempTag,
    size: u64,
}

pub(super) struct ImportedCollection {
    pub(super) temp_tag: TempTag,
    pub(super) file_count: u64,
    pub(super) size: u64,
    _collection: Collection,
}

/// 将已经标准化的路径转换为库内部使用的字符串表示，路径分隔使用 `/`。
///
/// - `must_be_relative`：如果为 true，则遇到根目录将返回错误（要求相对路径）。
pub fn canonicalized_path_to_string(
    path: impl AsRef<Path>,
    must_be_relative: bool,
) -> anyhow::Result<String> {
    let mut path_str = String::new();
    let parts = path
        .as_ref()
        .components()
        .filter_map(|c| match c {
            Component::Normal(x) => {
                let c = match x.to_str() {
                    Some(c) => c,
                    None => return Some(Err(anyhow::anyhow!("invalid character in path"))),
                };

                if !c.contains('/') && !c.contains('\\') {
                    Some(Ok(c))
                } else {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                }
            }
            Component::RootDir => {
                if must_be_relative {
                    Some(Err(anyhow::anyhow!("invalid path component {:?}", c)))
                } else {
                    path_str.push('/');
                    None
                }
            }
            _ => Some(Err(anyhow::anyhow!("invalid path component {:?}", c))),
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let parts = parts.join("/");
    path_str.push_str(&parts);
    Ok(path_str)
}
