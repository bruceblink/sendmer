use super::{RunningSend, round_trip_manifest};
#[test]
fn send_recv_manifest_preserves_empty_directory_and_metadata() {
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("manifest-share");
    let empty = source_root.join("empty").join("nested");
    let payload = source_root.join("readme.txt");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(&payload, b"manifest payload").unwrap();
    let source_time = filetime::FileTime::from_unix_time(1_700_000_000, 0);
    filetime::set_file_mtime(&payload, source_time).unwrap();

    let mut send = RunningSend::spawn_with_manifest(&source_root, src_dir.path()).unwrap();
    let ticket = send.read_ticket();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let options = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: sendmer::RelayModeOption::Disabled,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let result = runtime
        .block_on(sendmer::receive_with_events(
            ticket.to_string(),
            options,
            None,
        ))
        .expect("manifest receive should succeed");
    send.cleanup();

    assert_eq!(result.file_path, tgt_dir.path().join("manifest-share"));
    let received_payload = tgt_dir.path().join("manifest-share/readme.txt");
    assert_eq!(
        std::fs::read(&received_payload).unwrap(),
        b"manifest payload"
    );
    assert_eq!(
        filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(received_payload).unwrap()
        )
        .unix_seconds(),
        source_time.unix_seconds()
    );
    assert!(
        tgt_dir.path().join("manifest-share/empty/nested").is_dir(),
        "manifest mode must materialize empty directories"
    );
}

#[test]
fn send_recv_manifest_preserves_empty_root_directory() {
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("empty-manifest-share");
    std::fs::create_dir_all(&source_root).unwrap();

    let mut send = RunningSend::spawn_with_manifest(&source_root, src_dir.path()).unwrap();
    let ticket = send.read_ticket();
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let options = sendmer::ReceiveOptions {
        output_dir: Some(tgt_dir.path().to_path_buf()),
        relay_mode: sendmer::RelayModeOption::Disabled,
        magic_ipv4_addr: None,
        magic_ipv6_addr: None,
        retry_policy: Default::default(),
        receive_cache: None,
    };
    let result = runtime
        .block_on(sendmer::receive_with_events(
            ticket.to_string(),
            options,
            None,
        ))
        .expect("empty root manifest receive should succeed");
    send.cleanup();

    assert_eq!(
        result.file_path,
        tgt_dir.path().join("empty-manifest-share")
    );
    assert!(result.file_path.is_dir());
}

#[cfg(unix)]
#[test]
fn send_recv_manifest_preserves_posix_mode() {
    use std::os::unix::fs::PermissionsExt;

    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("mode-manifest-share");
    let payload = source_root.join("private.txt");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::write(&payload, b"mode payload").unwrap();
    std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o640)).unwrap();

    let received_root = round_trip_manifest(&source_root, src_dir.path(), tgt_dir.path());
    let received_mode = std::fs::metadata(received_root.join("private.txt"))
        .unwrap()
        .permissions()
        .mode()
        & 0o7777;
    assert_eq!(received_mode, 0o640);
}

#[cfg(windows)]
#[allow(clippy::permissions_set_readonly_false)]
#[test]
fn send_recv_manifest_preserves_windows_read_only_attribute() {
    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("readonly-manifest-share");
    let payload = source_root.join("readonly.txt");
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::write(&payload, b"readonly payload").unwrap();
    let mut source_permissions = std::fs::metadata(&payload).unwrap().permissions();
    source_permissions.set_readonly(true);
    std::fs::set_permissions(&payload, source_permissions).unwrap();

    let received_root = round_trip_manifest(&source_root, src_dir.path(), tgt_dir.path());
    let received_payload = received_root.join("readonly.txt");
    let received_readonly = std::fs::metadata(&received_payload)
        .unwrap()
        .permissions()
        .readonly();

    // Restore write access so temporary-directory cleanup is deterministic on Windows.
    let mut target_permissions = std::fs::metadata(&received_payload).unwrap().permissions();
    target_permissions.set_readonly(false);
    std::fs::set_permissions(&received_payload, target_permissions).unwrap();
    let mut source_permissions = std::fs::metadata(&payload).unwrap().permissions();
    source_permissions.set_readonly(false);
    std::fs::set_permissions(&payload, source_permissions).unwrap();

    assert!(
        received_readonly,
        "manifest receive must restore the Windows read-only attribute"
    );
}

// Linux preserves arbitrary Unix filename bytes; macOS filesystems reject this fixture with EILSEQ.
#[cfg(target_os = "linux")]
#[test]
fn send_recv_manifest_round_trips_non_utf8_filename() {
    use std::os::unix::ffi::OsStringExt;

    let src_dir = tempfile::tempdir().unwrap();
    let tgt_dir = tempfile::tempdir().unwrap();
    let source_root = src_dir.path().join("raw-name-manifest-share");
    let raw_name = std::ffi::OsString::from_vec(vec![b'c', 0xff, b'.', b't', b'x', b't']);
    let payload = source_root.join(&raw_name);
    std::fs::create_dir_all(&source_root).unwrap();
    std::fs::write(&payload, b"raw name payload").unwrap();

    let received_root = round_trip_manifest(&source_root, src_dir.path(), tgt_dir.path());
    assert_eq!(
        std::fs::read(received_root.join(raw_name)).unwrap(),
        b"raw name payload"
    );
}
