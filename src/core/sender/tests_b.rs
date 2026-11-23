#[tokio::test]
async fn send_rejects_empty_directory_during_preparation() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let empty = temp_dir.path().join("empty-share");
    std::fs::create_dir_all(&empty).expect("create empty directory");
    let emitter = Arc::new(RecordingEmitter::default());

    let result = send(
        empty,
        SendOptions {
            relay_mode: RelayModeOption::Disabled,
            ..SendOptions::default()
        },
        Some(emitter.clone()),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("empty directory should fail before sender setup"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("empty directory"));
    let events = emitter.events();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].event, TransferEventData::Started));
    assert!(matches!(
        &events[1].event,
        TransferEventData::Failed { error }
            if error.code == TransferErrorCode::InvalidInput
                && error.phase == TransferPhase::Preparing
                && !error.retryable
    ));
}

#[tokio::test]
async fn send_rejects_file_limit_before_network_setup() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let share = temp_dir.path().join("share");
    std::fs::create_dir_all(&share).expect("create share directory");
    std::fs::write(share.join("one.txt"), b"one").expect("write first file");
    std::fs::write(share.join("two.txt"), b"two").expect("write second file");
    let emitter = Arc::new(RecordingEmitter::default());

    let result = send(
        share,
        SendOptions {
            relay_mode: RelayModeOption::Disabled,
            max_files: NonZeroU64::new(1),
            ..SendOptions::default()
        },
        Some(emitter.clone()),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("file limit should fail before sender setup"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("more than 1 files"));
    let events = emitter.events();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].event, TransferEventData::Started));
    assert!(matches!(
        &events[1].event,
        TransferEventData::Failed { error }
            if error.code == TransferErrorCode::InvalidInput
                && error.phase == TransferPhase::Preparing
                && !error.retryable
    ));
}

#[tokio::test]
async fn send_rejects_total_size_limit_before_network_setup() {
    let temp_dir = tempfile::tempdir().expect("temp dir");
    let share = temp_dir.path().join("share");
    std::fs::create_dir_all(&share).expect("create share directory");
    std::fs::write(share.join("payload.bin"), b"payload").expect("write payload");
    let emitter = Arc::new(RecordingEmitter::default());

    let result = send(
        share,
        SendOptions {
            relay_mode: RelayModeOption::Disabled,
            max_total_size_bytes: NonZeroU64::new(6),
            ..SendOptions::default()
        },
        Some(emitter.clone()),
    )
    .await;
    let error = match result {
        Ok(_) => panic!("total size limit should fail before sender setup"),
        Err(error) => error,
    };

    assert!(error.to_string().contains("more than 6 bytes"));
    let events = emitter.events();
    assert_eq!(events.len(), 2);
    assert!(matches!(events[0].event, TransferEventData::Started));
    assert!(matches!(
        &events[1].event,
        TransferEventData::Failed { error }
            if error.code == TransferErrorCode::InvalidInput
                && error.phase == TransferPhase::Preparing
                && !error.retryable
    ));
}

#[cfg(unix)]
#[test]
fn validate_share_path_rejects_symbolic_link_source() {
    use std::os::unix::fs::symlink;

    let temp_dir = tempfile::tempdir().expect("temp dir");
    let source = temp_dir.path().join("source.txt");
    let link = temp_dir.path().join("source-link.txt");
    std::fs::write(&source, b"content").expect("write source file");
    symlink(&source, &link).expect("create source link");

    let error =
        validate_share_path(&link, false).expect_err("symbolic link should be rejected");

    assert!(error.to_string().contains("symbolic link"));
}

#[tokio::test]
async fn sender_setup_failure_removes_blob_store() {
    let source_dir = tempfile::tempdir().expect("source directory");
    let missing_source = source_dir.path().join("missing-source.bin");
    let storage_root = tempfile::tempdir().expect("storage root");
    let blobs_data_dir = storage_root.path().join("sender-store");
    let options = SendOptions {
        relay_mode: RelayModeOption::Disabled,
        ticket_type: AddrInfoOptions::RelayAndAddresses,
        ..SendOptions::default()
    };
    let endpoint = prepare_endpoint(&options)
        .await
        .expect("create sender endpoint");
    let share_request = ShareRequest {
        path: missing_source,
        entry_type: EntryType::File,
        event_emitter: started_sender_emitter(),
        max_upload_rate_bytes_per_sec: None,
        max_receivers: None,
        max_files: None,
        max_total_size_bytes: None,
        max_import_memory_bytes: None,
        manifest_mode: false,
    };

    let error = match setup_data_sharing(endpoint, blobs_data_dir.clone(), share_request).await
    {
        Ok(_) => panic!("missing source should fail sender setup"),
        Err(error) => error,
    };

    assert!(!error.to_string().is_empty());
    assert!(
        !blobs_data_dir.exists(),
        "sender setup failure should remove its blob store"
    );
}

#[tokio::test]
async fn cancelling_after_router_starts_removes_blob_store() {
    let source_dir = tempfile::tempdir().expect("source directory");
    let source_file = source_dir.path().join("source.bin");
    tokio::fs::write(&source_file, b"cancel after setup")
        .await
        .expect("write source file");
    let storage_root = tempfile::tempdir().expect("storage root");
    let blobs_data_dir = storage_root.path().join("sender-store");
    let options = SendOptions {
        relay_mode: RelayModeOption::Disabled,
        ticket_type: AddrInfoOptions::RelayAndAddresses,
        ..SendOptions::default()
    };
    let endpoint = prepare_endpoint(&options)
        .await
        .expect("create sender endpoint");
    let setup = setup_data_sharing(
        endpoint,
        blobs_data_dir.clone(),
        ShareRequest {
            path: source_file,
            entry_type: EntryType::File,
            event_emitter: started_sender_emitter(),
            max_upload_rate_bytes_per_sec: None,
            max_receivers: None,
            max_files: None,
            max_total_size_bytes: None,
            max_import_memory_bytes: None,
            manifest_mode: false,
        },
    )
    .await
    .expect("start sender router");

    let error = shutdown_started_sender_setup(
        setup,
        EntryType::File,
        AddrInfoOptions::RelayAndAddresses,
        started_sender_emitter(),
        anyhow::anyhow!("Operation cancelled"),
    )
    .await;

    assert!(error.to_string().contains("Operation cancelled"));
    assert!(
        !blobs_data_dir.exists(),
        "cancelling a started sender should remove its blob store"
    );
}

#[tokio::test]
async fn sender_setup_reports_source_size_after_stream_import() {
    let source_dir = tempfile::tempdir().expect("source directory");
    let source_file = source_dir.path().join("source.bin");
    let payload = b"stream import size";
    tokio::fs::write(&source_file, payload)
        .await
        .expect("write source file");
    let storage_root = tempfile::tempdir().expect("storage root");
    let blobs_data_dir = storage_root.path().join("sender-store");
    let options = SendOptions {
        relay_mode: RelayModeOption::Disabled,
        ticket_type: AddrInfoOptions::RelayAndAddresses,
        ..SendOptions::default()
    };
    let endpoint = prepare_endpoint(&options)
        .await
        .expect("create sender endpoint");
    let setup = setup_data_sharing(
        endpoint,
        blobs_data_dir,
        ShareRequest {
            path: source_file,
            entry_type: EntryType::File,
            event_emitter: started_sender_emitter(),
            max_upload_rate_bytes_per_sec: None,
            max_receivers: None,
            max_files: None,
            max_total_size_bytes: None,
            max_import_memory_bytes: None,
            manifest_mode: false,
        },
    )
    .await
    .expect("start sender setup");

    assert_eq!(setup.imported.size, payload.len() as u64);
    let error = shutdown_started_sender_setup(
        setup,
        EntryType::File,
        AddrInfoOptions::RelayAndAddresses,
        started_sender_emitter(),
        anyhow::anyhow!("size regression test cleanup"),
    )
    .await;
    assert!(error.to_string().contains("size regression test cleanup"));
}

fn started_sender_emitter() -> TransferEventEmitter {
    let emitter = TransferEventEmitter::new(None, Role::Sender);
    emitter.emit_started(TransferPhase::Preparing);
    emitter
}