use super::*;
#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::path::PathBuf;

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

#[test]
fn directory_manifest_rejects_file_at_root_path() {
    let mut manifest = empty_manifest();
    manifest.entries.push(ManifestEntry {
        id: "00000001".to_owned(),
        kind: ManifestEntryKind::File,
        path: utf8_path(&["share"]),
        payload: Some(ManifestPayload {
            collection_name: format!("{MANIFEST_ENTRY_PREFIX}00000001"),
            size: 1,
        }),
        metadata: ManifestMetadata::default(),
    });
    let error = manifest
        .validate()
        .expect_err("directory root cannot be a file entry");
    assert!(error.to_string().contains("root cannot also be a file"));
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

#[test]
fn manifest_parser_rejects_oversized_json_before_deserialization() {
    let bytes = vec![b' '; MAX_MANIFEST_BYTES + 1];
    let error = TransferManifest::from_json_bytes(&bytes)
        .expect_err("oversized manifest must be rejected before parsing");
    assert!(error.to_string().contains("exceeds"));
}

#[test]
fn manifest_validation_rejects_entry_count_over_limit() {
    let mut manifest = empty_manifest();
    manifest.entries = vec![manifest.entries[0].clone(); MAX_MANIFEST_ENTRIES + 1];
    let error = manifest
        .validate()
        .expect_err("oversized manifest entry list must be rejected");
    assert!(error.to_string().contains("too many entries"));
}

#[test]
fn manifest_path_validation_rejects_depth_and_component_size_over_limit() {
    let deep_path = ManifestPath {
        encoding: PathEncoding::Utf8,
        components: vec!["part".to_owned(); MAX_MANIFEST_PATH_DEPTH + 1],
    };
    let deep_error = deep_path
        .validate()
        .expect_err("deep manifest path must be rejected");
    assert!(deep_error.to_string().contains("depth limit"));

    let long_component = ManifestPath {
        encoding: PathEncoding::Utf8,
        components: vec!["x".repeat(MAX_MANIFEST_COMPONENT_BYTES + 1)],
    };
    let component_error = long_component
        .validate()
        .expect_err("long manifest component must be rejected");
    assert!(component_error.to_string().contains("component exceeds"));

    let long_path = ManifestPath {
        encoding: PathEncoding::Utf8,
        components: vec!["x".repeat(MAX_MANIFEST_COMPONENT_BYTES); 9],
    };
    let path_error = long_path
        .validate()
        .expect_err("long manifest path must be rejected");
    assert!(path_error.to_string().contains("path exceeds"));
}

#[test]
fn encoded_manifest_path_rejects_dot_and_parent_components() {
    for component in ["2e", "2e2e", "2e00", "2e002e00"] {
        let path = ManifestPath {
            encoding: PathEncoding::UnixBytesHex,
            components: vec!["7368617265".to_owned(), component.to_owned()],
        };
        assert!(path.validate().is_err(), "encoded traversal {component}");
    }
}

#[test]
fn manifest_validation_rejects_payload_size_overflow() {
    let mut manifest = empty_manifest();
    manifest.entries = vec![
        ManifestEntry {
            id: "00000000".to_owned(),
            kind: ManifestEntryKind::Directory,
            path: utf8_path(&["share", "nested"]),
            payload: None,
            metadata: ManifestMetadata::default(),
        },
        ManifestEntry {
            id: "00000001".to_owned(),
            kind: ManifestEntryKind::File,
            path: utf8_path(&["share", "nested", "first.bin"]),
            payload: Some(ManifestPayload {
                collection_name: format!("{MANIFEST_ENTRY_PREFIX}00000001"),
                size: u64::MAX,
            }),
            metadata: ManifestMetadata::default(),
        },
        ManifestEntry {
            id: "00000002".to_owned(),
            kind: ManifestEntryKind::File,
            path: utf8_path(&["share", "nested", "second.bin"]),
            payload: Some(ManifestPayload {
                collection_name: format!("{MANIFEST_ENTRY_PREFIX}00000002"),
                size: 1,
            }),
            metadata: ManifestMetadata::default(),
        },
    ];

    let error = manifest
        .validate()
        .expect_err("payload sizes that overflow u64 must be rejected");
    assert!(error.to_string().contains("payload size"));
}

#[test]
fn manifest_validation_rejects_unsupported_posix_mode_bits() {
    let mut manifest = empty_manifest();
    manifest.entries[0].metadata.permissions = Some(ManifestPermissions {
        posix_mode: Some(0o10_000),
        windows_read_only: None,
    });

    let error = manifest
        .validate()
        .expect_err("POSIX mode bits outside 0o7777 must be rejected");
    assert!(error.to_string().contains("POSIX mode"));
}

#[test]
fn windows_utf8_component_rejects_device_names_with_extensions() {
    for component in [
        "CON",
        "con.txt",
        "PrN.log",
        "AUX.data",
        "nul.bin",
        "COM1.txt",
        "com9.backup",
        "LPT1.csv",
        "CLOCK$.log",
        "CONIN$.tmp",
        "CONOUT$.tmp",
    ] {
        assert!(
            validate_windows_utf8_component(component).is_err(),
            "{component} must be rejected as a Windows device name"
        );
    }
    for component in ["COM10.txt", "LPT0.txt", "console.txt", "report.CON.txt"] {
        validate_windows_utf8_component(component)
            .unwrap_or_else(|error| panic!("{component} should be accepted: {error}"));
    }
}

#[cfg(windows)]
#[test]
fn windows_utf16_component_rejects_device_names_with_extensions() {
    for component in ["cOm1.txt", "lPt9.backup", "nUl.data"] {
        let wide = component.encode_utf16().collect::<Vec<_>>();
        assert!(
            validate_windows_wide_component(&wide).is_err(),
            "{component} must be rejected as a Windows device name"
        );
    }
    let allowed = "COM10.txt".encode_utf16().collect::<Vec<_>>();
    validate_windows_wide_component(&allowed).expect("COM10 is not a reserved Windows device name");
}
