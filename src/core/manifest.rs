//! Versioned transfer manifest primitives for v0.10 directory semantics.
//!
//! The legacy iroh Collection stores UTF-8 names only. TM1 keeps the logical
//! tree and platform metadata in a separately validated JSON payload so the
//! receiver can reject unsafe or unsupported entries before touching staging.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

/// The highest transfer-manifest schema understood by this crate.
pub const MANIFEST_SCHEMA_VERSION: u16 = 1;

/// Reserved iroh Collection name for the raw TM1 JSON blob.
pub const MANIFEST_COLLECTION_NAME: &str = ".sendmer/manifest-v1";

/// Prefix for deterministic payload names in a manifest-mode collection.
pub const MANIFEST_ENTRY_PREFIX: &str = ".sendmer/entry/";

/// A versioned logical tree carried alongside payload blobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferManifest {
    pub schema_version: u16,
    pub root: ManifestRoot,
    pub entries: Vec<ManifestEntry>,
}

/// The one user-visible root represented by a transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRoot {
    pub kind: ManifestEntryKind,
    pub path: ManifestPath,
}

/// A logical file or directory in the transfer tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub id: String,
    pub kind: ManifestEntryKind,
    pub path: ManifestPath,
    pub payload: Option<ManifestPayload>,
    pub metadata: ManifestMetadata,
}

/// Whether a manifest entry is a regular file or directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestEntryKind {
    File,
    Directory,
}

/// The collection mapping and declared byte length for one file payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPayload {
    pub collection_name: String,
    pub size: u64,
}

/// Metadata that can be applied after all payloads have been verified.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestMetadata {
    pub permissions: Option<ManifestPermissions>,
    pub modified: Option<UnixTimestamp>,
}

/// Portable subset of platform permissions supported by TM1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestPermissions {
    pub posix_mode: Option<u32>,
    pub windows_read_only: Option<bool>,
}

/// Signed Unix timestamp with an explicit nanosecond remainder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnixTimestamp {
    pub seconds: i64,
    pub nanos: u32,
}

/// A path whose components are losslessly represented on the sending platform.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ManifestPath {
    pub encoding: PathEncoding,
    pub components: Vec<String>,
}

/// Wire encoding used for every component in one path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathEncoding {
    Utf8,
    UnixBytesHex,
    WindowsUtf16LeHex,
}

impl TransferManifest {
    /// Validate all schema, path, mapping, and metadata invariants before encoding or export.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.schema_version <= MANIFEST_SCHEMA_VERSION,
            "unsupported transfer manifest schema version {}",
            self.schema_version
        );
        anyhow::ensure!(
            self.schema_version > 0,
            "transfer manifest schema version must be greater than zero"
        );
        self.root.path.validate()?;
        anyhow::ensure!(
            !self.root.path.components.is_empty(),
            "transfer manifest root path must not be empty"
        );

        let mut paths = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut payload_names = BTreeSet::new();
        for (index, entry) in self.entries.iter().enumerate() {
            anyhow::ensure!(
                entry.id == format!("{index:08x}"),
                "manifest entry IDs must be contiguous lowercase hexadecimal values"
            );
            anyhow::ensure!(ids.insert(entry.id.clone()), "duplicate manifest entry ID");
            entry.path.validate()?;
            anyhow::ensure!(
                entry.path.encoding == self.root.path.encoding,
                "manifest paths must use one path encoding"
            );
            anyhow::ensure!(
                has_prefix(&entry.path.components, &self.root.path.components),
                "manifest entry path is outside the root"
            );
            anyhow::ensure!(
                paths.insert(entry.path.clone()),
                "duplicate manifest entry path"
            );
            validate_entry_payload(entry, &mut payload_names)?;
            validate_metadata(&entry.metadata)?;
        }

        if self.root.kind == ManifestEntryKind::File {
            anyhow::ensure!(
                self.entries.len() == 1
                    && self.entries[0].kind == ManifestEntryKind::File
                    && self.entries[0].path == self.root.path,
                "file manifest must contain exactly one root file entry"
            );
        }

        validate_tree_shape(self)?;
        Ok(())
    }

    /// Serialize a validated manifest as deterministic, human-inspectable JSON bytes.
    pub fn to_json_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec(self).map_err(Into::into)
    }

    /// Parse and validate a manifest received from an untrusted peer.
    pub fn from_json_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| anyhow::anyhow!("invalid transfer manifest JSON: {error}"))?;
        manifest.validate()?;
        Ok(manifest)
    }
}

impl ManifestPath {
    /// Choose the one encoding needed to represent every component in a source tree.
    pub fn encoding_for_paths(paths: impl IntoIterator<Item = impl AsRef<Path>>) -> PathEncoding {
        #[cfg(unix)]
        {
            if paths.into_iter().any(|path| {
                path.as_ref()
                    .components()
                    .any(|component| matches!(component, Component::Normal(value) if value.to_str().is_none()))
            }) {
                PathEncoding::UnixBytesHex
            } else {
                PathEncoding::Utf8
            }
        }
        #[cfg(windows)]
        {
            if paths.into_iter().any(|path| {
                path.as_ref()
                    .components()
                    .any(|component| matches!(component, Component::Normal(value) if value.to_str().is_none()))
            }) {
                PathEncoding::WindowsUtf16LeHex
            } else {
                PathEncoding::Utf8
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = paths;
            PathEncoding::Utf8
        }
    }

    /// Encode a platform path without silently replacing an unrepresentable name.
    pub fn from_path(path: &Path, must_be_relative: bool) -> anyhow::Result<Self> {
        let encoding = Self::encoding_for_paths([path]);
        Self::from_path_with_encoding(path, must_be_relative, encoding)
    }

    /// Encode a path using a tree-wide encoding selected by `encoding_for_paths`.
    pub fn from_path_with_encoding(
        path: &Path,
        must_be_relative: bool,
        encoding: PathEncoding,
    ) -> anyhow::Result<Self> {
        let components = path
            .components()
            .map(|component| match component {
                Component::Normal(value) => Ok(value),
                Component::RootDir if !must_be_relative => Ok(OsStr::new("/")),
                other => Err(anyhow::anyhow!("invalid manifest path component {other:?}")),
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        anyhow::ensure!(
            !components.is_empty(),
            "manifest path must contain at least one component"
        );

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let components = components
                .into_iter()
                .map(|component| match encoding {
                    PathEncoding::Utf8 => component
                        .to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("path component is not valid UTF-8")),
                    PathEncoding::UnixBytesHex => {
                        Ok(data_encoding::HEXLOWER.encode(component.as_bytes()))
                    }
                    PathEncoding::WindowsUtf16LeHex => unreachable!(),
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Self {
                encoding,
                components,
            })
        }

        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let components = components
                .into_iter()
                .map(|component| match encoding {
                    PathEncoding::Utf8 => component
                        .to_str()
                        .map(str::to_owned)
                        .ok_or_else(|| anyhow::anyhow!("path component is not valid UTF-8")),
                    PathEncoding::WindowsUtf16LeHex => {
                        let bytes = component
                            .encode_wide()
                            .flat_map(u16::to_le_bytes)
                            .collect::<Vec<_>>();
                        Ok(data_encoding::HEXLOWER.encode(&bytes))
                    }
                    PathEncoding::UnixBytesHex => unreachable!(),
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(Self {
                encoding,
                components,
            })
        }

        #[cfg(not(any(unix, windows)))]
        {
            anyhow::ensure!(
                components
                    .iter()
                    .all(|component| component.to_str().is_some()),
                "platform cannot encode non-UTF-8 manifest paths"
            );
            Ok(Self {
                encoding: PathEncoding::Utf8,
                components: components
                    .into_iter()
                    .map(|component| component.to_string_lossy().into_owned())
                    .collect(),
            })
        }
    }

    /// Validate wire components without materializing them on the filesystem.
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.components.is_empty(),
            "manifest path must not be empty"
        );
        for component in &self.components {
            anyhow::ensure!(!component.is_empty(), "manifest path component is empty");
            anyhow::ensure!(
                !component.contains('/') && !component.contains('\\'),
                "manifest path component contains a separator"
            );
            match self.encoding {
                PathEncoding::Utf8 => {
                    anyhow::ensure!(
                        component != "." && component != "..",
                        "invalid path component"
                    )
                }
                PathEncoding::UnixBytesHex | PathEncoding::WindowsUtf16LeHex => {
                    anyhow::ensure!(
                        component
                            .bytes()
                            .all(|byte| { byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte) })
                            && component.len().is_multiple_of(2),
                        "encoded manifest path component is not lowercase hex"
                    );
                }
            }
        }
        Ok(())
    }

    /// Decode a validated path for this host, failing instead of lossy conversion.
    pub fn to_path_buf(&self) -> anyhow::Result<PathBuf> {
        self.validate()?;
        match self.encoding {
            PathEncoding::Utf8 => {
                #[cfg(windows)]
                for component in &self.components {
                    validate_windows_utf8_component(component)?;
                }
                Ok(self.components.iter().collect())
            }
            PathEncoding::UnixBytesHex => {
                #[cfg(unix)]
                {
                    use std::os::unix::ffi::OsStringExt;
                    let mut path = PathBuf::new();
                    for component in &self.components {
                        let bytes = data_encoding::HEXLOWER
                            .decode(component.as_bytes())
                            .map_err(|_| anyhow::anyhow!("invalid Unix path component hex"))?;
                        anyhow::ensure!(
                            !bytes.contains(&0)
                                && !bytes.contains(&b'/')
                                && !bytes.contains(&b'\\'),
                            "decoded Unix path component contains an invalid byte"
                        );
                        path.push(OsString::from_vec(bytes));
                    }
                    Ok(path)
                }
                #[cfg(not(unix))]
                {
                    anyhow::bail!("Unix byte path encoding is unsupported on this platform")
                }
            }
            PathEncoding::WindowsUtf16LeHex => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStringExt;
                    let mut path = PathBuf::new();
                    for component in &self.components {
                        let bytes = data_encoding::HEXLOWER
                            .decode(component.as_bytes())
                            .map_err(|_| anyhow::anyhow!("invalid Windows path component hex"))?;
                        anyhow::ensure!(
                            bytes.len().is_multiple_of(2),
                            "Windows UTF-16 path bytes must be even"
                        );
                        let wide = bytes
                            .chunks_exact(2)
                            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                            .collect::<Vec<_>>();
                        validate_windows_wide_component(&wide)?;
                        path.push(OsString::from_wide(&wide));
                    }
                    Ok(path)
                }
                #[cfg(not(windows))]
                {
                    anyhow::bail!("Windows UTF-16 path encoding is unsupported on this platform")
                }
            }
        }
    }
}

#[cfg(windows)]
fn validate_windows_utf8_component(component: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !component.bytes().any(|byte| matches!(
            byte,
            b'<' | b'>' | b':' | b'"' | b'/' | b'\\' | b'|' | b'?' | b'*'
        )),
        "manifest path component contains a Windows-invalid character"
    );
    anyhow::ensure!(
        !component.ends_with('.') && !component.ends_with(' '),
        "manifest path component cannot end with a dot or space"
    );
    Ok(())
}

#[cfg(windows)]
fn validate_windows_wide_component(component: &[u16]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !component.iter().any(|unit| {
            *unit <= 0x001f
                || matches!(
                    *unit,
                    0x003c | 0x003e | 0x003a | 0x0022 | 0x002f | 0x005c | 0x007c | 0x003f | 0x002a
                )
        }),
        "decoded Windows path component contains an invalid character"
    );
    anyhow::ensure!(
        component
            .last()
            .is_none_or(|unit| *unit != u16::from(b'.') && *unit != u16::from(b' ')),
        "decoded Windows path component cannot end with a dot or space"
    );
    Ok(())
}

fn has_prefix(path: &[String], prefix: &[String]) -> bool {
    path.len() >= prefix.len() && path.iter().zip(prefix).all(|(left, right)| left == right)
}

fn validate_entry_payload(
    entry: &ManifestEntry,
    payload_names: &mut BTreeSet<String>,
) -> anyhow::Result<()> {
    match (entry.kind, &entry.payload) {
        (ManifestEntryKind::File, Some(payload)) => {
            anyhow::ensure!(
                payload.collection_name.starts_with(MANIFEST_ENTRY_PREFIX),
                "manifest payload collection name is not reserved"
            );
            anyhow::ensure!(
                payload.collection_name == format!("{MANIFEST_ENTRY_PREFIX}{}", entry.id),
                "manifest payload collection name is invalid"
            );
            anyhow::ensure!(
                payload_names.insert(payload.collection_name.clone()),
                "duplicate manifest payload collection name"
            );
        }
        (ManifestEntryKind::File, None) => anyhow::bail!("file manifest entry is missing payload"),
        (ManifestEntryKind::Directory, Some(_)) => {
            anyhow::bail!("directory manifest entry cannot have a payload")
        }
        (ManifestEntryKind::Directory, None) => {}
    }
    Ok(())
}

fn validate_metadata(metadata: &ManifestMetadata) -> anyhow::Result<()> {
    if let Some(permissions) = &metadata.permissions {
        anyhow::ensure!(
            permissions.posix_mode.is_some() ^ permissions.windows_read_only.is_some(),
            "manifest permissions must contain exactly one platform representation"
        );
    }
    if let Some(modified) = metadata.modified {
        anyhow::ensure!(
            modified.nanos < 1_000_000_000,
            "manifest timestamp nanos are invalid"
        );
    }
    Ok(())
}

fn validate_tree_shape(manifest: &TransferManifest) -> anyhow::Result<()> {
    let mut files = BTreeSet::new();
    let mut directories = BTreeSet::from([manifest.root.path.clone()]);
    for entry in &manifest.entries {
        if entry.kind == ManifestEntryKind::File {
            files.insert(entry.path.clone());
        } else {
            directories.insert(entry.path.clone());
        }
    }
    for entry in &manifest.entries {
        let mut ancestor = entry.path.components.clone();
        ancestor.pop();
        while ancestor.len() >= manifest.root.path.components.len() {
            let path = ManifestPath {
                encoding: entry.path.encoding,
                components: ancestor.clone(),
            };
            anyhow::ensure!(
                !files.contains(&path),
                "file entry cannot contain another entry"
            );
            anyhow::ensure!(
                directories.contains(&path),
                "manifest entry is missing its directory parent"
            );
            if ancestor == manifest.root.path.components {
                break;
            }
            ancestor.pop();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf8_path(parts: &[&str]) -> ManifestPath {
        ManifestPath {
            encoding: PathEncoding::Utf8,
            components: parts.iter().map(|part| (*part).to_owned()).collect(),
        }
    }

    fn empty_manifest() -> TransferManifest {
        TransferManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            root: ManifestRoot {
                kind: ManifestEntryKind::Directory,
                path: utf8_path(&["share"]),
            },
            entries: vec![ManifestEntry {
                id: "00000000".to_owned(),
                kind: ManifestEntryKind::Directory,
                path: utf8_path(&["share", "empty"]),
                payload: None,
                metadata: ManifestMetadata::default(),
            }],
        }
    }

    #[test]
    fn manifest_round_trips_json_and_preserves_empty_directory() {
        let manifest = empty_manifest();
        let encoded = manifest.to_json_bytes().expect("encode manifest");
        let decoded = TransferManifest::from_json_bytes(&encoded).expect("decode manifest");
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn future_manifest_schema_is_rejected_before_export() {
        let mut manifest = empty_manifest();
        manifest.schema_version = MANIFEST_SCHEMA_VERSION + 1;
        let error = manifest
            .validate()
            .expect_err("future schema must be rejected");
        assert!(
            error
                .to_string()
                .contains("unsupported transfer manifest schema")
        );
    }

    #[test]
    fn duplicate_paths_and_file_children_are_rejected() {
        let mut manifest = empty_manifest();
        manifest.entries[0].kind = ManifestEntryKind::File;
        manifest.entries[0].payload = Some(ManifestPayload {
            collection_name: format!("{MANIFEST_ENTRY_PREFIX}00000000"),
            size: 1,
        });
        manifest.entries.push(ManifestEntry {
            id: "00000001".to_owned(),
            kind: ManifestEntryKind::File,
            path: utf8_path(&["share", "empty", "nested.bin"]),
            payload: Some(ManifestPayload {
                collection_name: format!("{MANIFEST_ENTRY_PREFIX}00000001"),
                size: 1,
            }),
            metadata: ManifestMetadata::default(),
        });
        let error = manifest
            .validate()
            .expect_err("directory/file collision must fail");
        assert!(error.to_string().contains("file entry cannot contain"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_non_utf8_path_uses_lossless_hex_encoding() {
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(vec![b'n', b'a', b'm', b'e', 0xff]));
        let encoded = ManifestPath::from_path(&path, true).expect("encode raw Unix name");
        assert_eq!(encoded.encoding, PathEncoding::UnixBytesHex);
        assert_eq!(encoded.to_path_buf().expect("decode raw Unix name"), path);
    }

    #[test]
    fn path_validation_rejects_traversal_and_separator_components() {
        let traversal = ManifestPath {
            encoding: PathEncoding::Utf8,
            components: vec!["share".to_owned(), "..".to_owned()],
        };
        assert!(traversal.validate().is_err());
        let separator = ManifestPath {
            encoding: PathEncoding::Utf8,
            components: vec!["share/file".to_owned()],
        };
        assert!(separator.validate().is_err());
    }
}
