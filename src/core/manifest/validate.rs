use super::model::*;
use std::collections::BTreeSet;

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
        anyhow::ensure!(
            self.entries.len() <= MAX_MANIFEST_ENTRIES,
            "transfer manifest contains too many entries (maximum {MAX_MANIFEST_ENTRIES})"
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

        self.total_payload_size()?;

        if self.root.kind == ManifestEntryKind::File {
            anyhow::ensure!(
                self.entries.len() == 1
                    && self.entries[0].kind == ManifestEntryKind::File
                    && self.entries[0].path == self.root.path,
                "file manifest must contain exactly one root file entry"
            );
        } else {
            anyhow::ensure!(
                !self
                    .entries
                    .iter()
                    .any(|entry| entry.kind == ManifestEntryKind::File
                        && entry.path == self.root.path),
                "directory manifest root cannot also be a file entry"
            );
        }

        validate_tree_shape(self)?;
        Ok(())
    }

    /// Serialize a validated manifest as deterministic, human-inspectable JSON bytes.
    pub fn to_json_bytes(&self) -> anyhow::Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)?;
        anyhow::ensure!(
            bytes.len() <= MAX_MANIFEST_BYTES,
            "transfer manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit"
        );
        Ok(bytes)
    }

    /// Parse and validate a manifest received from an untrusted peer.
    pub fn from_json_bytes(bytes: &[u8]) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() <= MAX_MANIFEST_BYTES,
            "transfer manifest exceeds the {MAX_MANIFEST_BYTES}-byte limit"
        );
        let manifest: Self = serde_json::from_slice(bytes)
            .map_err(|error| anyhow::anyhow!("invalid transfer manifest JSON: {error}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// 返回清单中所有文件载荷大小的检查后总和。
    pub(crate) fn total_payload_size(&self) -> anyhow::Result<u64> {
        self.entries
            .iter()
            .filter_map(|entry| entry.payload.as_ref())
            .try_fold(0_u64, |total, payload| {
                total
                    .checked_add(payload.size)
                    .ok_or_else(|| anyhow::anyhow!("manifest payload size exceeds the u64 range"))
            })
    }
}
/// 按 Windows 路径规则检查一个路径组件。
///
/// 该函数不依赖当前平台。旧集合名称在写入文件系统前也使用同一检查。
pub fn validate_windows_utf8_component(component: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !component.chars().any(|character| {
            character <= '\u{001f}'
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        }),
        "manifest path component contains a Windows-invalid character"
    );
    anyhow::ensure!(
        !component.ends_with('.') && !component.ends_with(' '),
        "manifest path component cannot end with a dot or space"
    );
    anyhow::ensure!(
        !is_windows_device_name(component),
        "manifest path component is a reserved Windows device name"
    );
    Ok(())
}

/// 在不执行有损解码的前提下，按 Windows 规则检查 UTF-16 路径组件。
#[cfg(windows)]
pub fn validate_windows_wide_component(component: &[u16]) -> anyhow::Result<()> {
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
    anyhow::ensure!(
        !is_windows_wide_device_name(component),
        "decoded Windows path component is a reserved Windows device name"
    );
    Ok(())
}

/// 识别大小写不敏感且允许带扩展名的 Windows 保留设备名。
fn is_windows_device_name(component: &str) -> bool {
    let component = component.trim_end_matches(['.', ' ']);
    let base = component.split('.').next().unwrap_or(component);
    let base = base.to_ascii_uppercase();
    matches!(
        base.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || is_numbered_windows_device(&base, "COM")
        || is_numbered_windows_device(&base, "LPT")
}

/// 检查基础名称是否为 Windows 保留设备名。
fn is_numbered_windows_device(base: &str, prefix: &str) -> bool {
    let Some(number) = base.strip_prefix(prefix) else {
        return false;
    };
    matches!(
        number,
        "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
    )
}

/// 从 UTF-16 代码单元中识别保留设备名，扩展名不参与比较。
#[cfg(windows)]
fn is_windows_wide_device_name(component: &[u16]) -> bool {
    let mut ascii = String::with_capacity(component.len());
    for unit in component
        .iter()
        .copied()
        .take_while(|unit| *unit != u16::from(b'.'))
    {
        match unit {
            0x0061..=0x007a => ascii.push((unit as u8).to_ascii_uppercase() as char),
            0x0041..=0x005a | 0x0030..=0x0039 => ascii.push(unit as u8 as char),
            0x00b9 => ascii.push('¹'),
            0x00b2 => ascii.push('²'),
            0x00b3 => ascii.push('³'),
            0x0020 => ascii.push(' '),
            0x0024 => ascii.push('$'),
            _ => return false,
        }
    }
    is_windows_device_name(&ascii)
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
        if let Some(mode) = permissions.posix_mode {
            anyhow::ensure!(
                mode <= 0o7777,
                "manifest POSIX mode contains unsupported bits"
            );
        }
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
