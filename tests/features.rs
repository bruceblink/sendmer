//! Disabled capabilities must fail closed without network or filesystem side effects.

#[cfg(not(feature = "manifest"))]
#[test]
fn minimal_build_rejects_tm1_send_options() {
    let options = sendmer::SendOptions {
        manifest_mode: true,
        ..Default::default()
    };
    let error = options
        .validate()
        .expect_err("TM1 must require its feature");
    assert!(error.to_string().contains("manifest feature"));
}

#[cfg(not(feature = "persistent-recovery"))]
#[tokio::test]
async fn disabled_cache_never_creates_or_prunes_storage() {
    use iroh_blobs::{BlobFormat, Hash, ticket::BlobTicket};
    use sendmer::{ReceiveCacheOptions, ReceiveOptions, RelayModeOption};

    let root = tempfile::tempdir().unwrap();
    let cache = root.path().join("untouched");
    let output = root.path().join("output");
    let ticket = BlobTicket::new(
        iroh::EndpointAddr::new(iroh::SecretKey::generate().public()),
        Hash::new(b"feature contract"),
        BlobFormat::HashSeq,
    );
    let options = ReceiveOptions {
        relay_mode: RelayModeOption::Disabled,
        output_dir: Some(output.clone()),
        receive_cache: Some(ReceiveCacheOptions::new(&cache)),
        ..Default::default()
    };
    assert!(sendmer::receive(ticket.to_string(), options).await.is_err());
    assert!(!cache.exists());
    assert!(!output.exists());

    std::fs::create_dir_all(&cache).unwrap();
    let sentinel = cache.join("keep");
    std::fs::write(&sentinel, b"untouched").unwrap();
    let error = sendmer::prune_receive_cache(&cache)
        .await
        .expect_err("cache prune must require its feature");
    assert!(error.to_string().contains("persistent-recovery feature"));
    assert_eq!(std::fs::read(sentinel).unwrap(), b"untouched");
}
