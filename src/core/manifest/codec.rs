use super::model::*;
#[cfg(windows)]
use super::validate::validate_windows_utf8_component;
#[cfg(windows)]
use super::validate::validate_windows_wide_component;
use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

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
        anyhow::ensure!(
            self.components.len() <= MAX_MANIFEST_PATH_DEPTH,
            "manifest path exceeds the {MAX_MANIFEST_PATH_DEPTH}-component depth limit"
        );
        let mut total_bytes = 0_usize;
        for component in &self.components {
            anyhow::ensure!(!component.is_empty(), "manifest path component is empty");
            anyhow::ensure!(
                component.len() <= MAX_MANIFEST_COMPONENT_BYTES,
                "manifest path component exceeds the {MAX_MANIFEST_COMPONENT_BYTES}-byte limit"
            );
            total_bytes = total_bytes
                .checked_add(component.len())
                .and_then(|size| size.checked_add(1))
                .ok_or_else(|| anyhow::anyhow!("manifest path length exceeds usize range"))?;
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
                    anyhow::ensure!(
                        component != "2e"
                            && component != "2e2e"
                            && component != "2e00"
                            && component != "2e002e00",
                        "encoded manifest path component cannot be traversal"
                    );
                }
            }
        }
        anyhow::ensure!(
            total_bytes.saturating_sub(1) <= MAX_MANIFEST_PATH_BYTES,
            "manifest path exceeds the {MAX_MANIFEST_PATH_BYTES}-byte limit"
        );
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
                        anyhow::ensure!(
                            bytes != b"." && bytes != b"..",
                            "decoded Unix path component cannot be traversal"
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
                        // The even-length check above guarantees one UTF-16 unit per byte pair.
                        let wide = bytes
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|pair| u16::from_le_bytes(*pair))
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
