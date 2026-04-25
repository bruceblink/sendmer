//! Shared temporary-directory and blob-store helpers.

use data_encoding::HEXLOWER;
use iroh_blobs::store::fs::FsStore;
use rand::Rng;
use std::path::{Path, PathBuf};

pub fn unique_temp_dir(prefix: &str) -> anyhow::Result<PathBuf> {
    let suffix = rand::rng().random::<[u8; 16]>();
    let path = std::env::temp_dir().join(format!("{prefix}{}", HEXLOWER.encode(&suffix)));

    if path.exists() {
        anyhow::bail!(
            "can not create a unique temporary directory twice at {}",
            path.display(),
        );
    }

    Ok(path)
}

pub async fn load_fs_store(path: &Path) -> anyhow::Result<FsStore> {
    tokio::fs::create_dir_all(path).await?;
    FsStore::load(path).await
}

#[cfg(test)]
mod tests {
    use super::unique_temp_dir;

    #[test]
    fn unique_temp_dir_generates_prefixed_path() {
        let path = unique_temp_dir(".sendmer-send-").expect("temp path");
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("utf-8 path");

        assert!(path.starts_with(std::env::temp_dir()));
        assert!(file_name.starts_with(".sendmer-send-"));
    }

    #[test]
    fn unique_temp_dir_returns_distinct_paths() {
        let first = unique_temp_dir(".sendmer-recv-").expect("first path");
        let second = unique_temp_dir(".sendmer-recv-").expect("second path");
        assert_ne!(first, second);
    }
}
