#[tokio::test]
async fn receive_cancellation_before_prepare_skips_ticket_parsing() {
    let (_sender, receiver) = tokio::sync::watch::channel(true);
    let error = receive_with_limits_and_cancellation(
        "not-a-blob-ticket".to_owned(),
        ReceiveOptions::default(),
        ReceiveLimits::default(),
        None,
        Some(receiver),
    )
    .await
    .expect_err("pre-cancelled receive should return cancellation");

    assert!(crate::core::events::is_transfer_cancelled(&error));
}

#[test]
fn retryable_get_stream_error_does_not_emit_terminal_event() {
    let emitter = Arc::new(RecordingEmitter::default());
    let app_handle: crate::core::events::AppHandle = Some(emitter.clone());

    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let mut s = stream::empty::<GetProgressItem>();
        let mut reporter = receiver_reporter(app_handle, 12);
        reporter.emit_initial_progress();
        let err = process_get_stream_with_reporter(&mut s, 0, &mut reporter, None)
            .await
            .expect_err("stream ending early should fail");
        assert!(err.to_string().contains("ended before completion"));
    });

    let events = emitter.events();
    assert!(matches!(
        events.get(1),
        Some(event)
            if event.role == Role::Receiver
                && matches!(
                    &event.event,
                    TransferEventData::Progress {
                        processed: 0,
                        total: 12,
                        ..
                    }
                )
    ));
    assert!(!events.iter().any(|event| event.event.is_terminal()));
}

#[test]
fn process_export_stream_rejects_early_end() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let mut s = stream::empty::<ExportProgressItem>();
        let err = process_export_stream(&mut s, "report.txt")
            .await
            .expect_err("stream ending early should fail");
        assert!(err.to_string().contains("ended before completion"));
    });
}

#[test]
fn process_export_stream_accepts_done() {
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    runtime.block_on(async {
        let mut s = stream::iter([ExportProgressItem::Done]);
        process_export_stream(&mut s, "report.txt")
            .await
            .expect("done item should complete export");
    });
}

#[test]
fn emit_receive_failed_emits_receiver_failed_event() {
    let emitter = Arc::new(RecordingEmitter::default());
    let app_handle: crate::core::events::AppHandle = Some(emitter.clone());
    let session = TransferEventEmitter::new(app_handle, Role::Receiver);
    session.emit_started(TransferPhase::Connecting);
    let failure = receive_failure(
        anyhow::anyhow!("connection refused"),
        TransferErrorCode::ConnectionFailed,
        TransferPhase::Connecting,
        true,
        "unable to connect to the sender",
    );

    emit_receive_failed(&session, &failure);

    let events = emitter.events();
    assert_eq!(events.len(), 2);
    match &events[1] {
        TransferEvent {
            role,
            event: TransferEventData::Failed { error },
            ..
        } => {
            assert_eq!(*role, Role::Receiver);
            assert_eq!(error.code, TransferErrorCode::ConnectionFailed);
            assert_eq!(error.phase, TransferPhase::Connecting);
            assert!(error.retryable);
            assert_eq!(error.message, "unable to connect to the sender");
        }
        other => panic!("expected failed event, got {other:?}"),
    }
}

#[test]
fn resolve_output_dir_resolves_relative_and_preserves_absolute_paths() {
    let dir = Path::new("explicit-dir").to_path_buf();
    let expected = std::env::current_dir().expect("current dir").join(&dir);
    let resolved = resolve_output_dir(Some(dir)).expect("relative output should resolve");
    assert_eq!(resolved, expected);

    let temp = tempfile::tempdir().expect("private output parent");
    let absolute = temp.path().join("not-created");
    let resolved =
        resolve_output_dir(Some(absolute.clone())).expect("absolute output should pass");
    assert_eq!(resolved, absolute);
    assert!(
        !absolute.exists(),
        "resolving must not create the destination"
    );
}

#[test]
fn resolve_output_dir_defaults_to_current_directory() {
    let expected = std::env::current_dir().expect("current dir");
    let resolved = resolve_output_dir(None).expect("default output should resolve");
    assert_eq!(resolved, expected);
}

#[test]
fn size_fetch_backoff_scales_by_attempt() {
    let policy = ReceiveRetryPolicy {
        size_fetch_backoff_ms: 125,
        ..Default::default()
    };

    assert_eq!(size_fetch_backoff(0, policy), std::time::Duration::ZERO);
    assert_eq!(
        size_fetch_backoff(2, policy),
        std::time::Duration::from_millis(250)
    );
    assert_eq!(
        size_fetch_backoff(
            u32::MAX,
            ReceiveRetryPolicy {
                size_fetch_backoff_ms: u64::MAX,
                ..Default::default()
            }
        ),
        std::time::Duration::from_secs(30)
    );
}

#[test]
fn download_backoff_scales_by_attempt() {
    let policy = ReceiveRetryPolicy {
        download_retry_backoff_ms: 75,
        ..Default::default()
    };

    assert_eq!(download_backoff(0, policy), std::time::Duration::ZERO);
    assert_eq!(
        download_backoff(3, policy),
        std::time::Duration::from_millis(225)
    );
    assert_eq!(
        download_backoff(
            u32::MAX,
            ReceiveRetryPolicy {
                download_retry_backoff_ms: u64::MAX,
                ..Default::default()
            }
        ),
        std::time::Duration::from_secs(30)
    );
}

#[tokio::test]
async fn close_receive_endpoint_marks_endpoint_closed() {
    let options = ReceiveOptions {
        relay_mode: RelayModeOption::Disabled,
        ..Default::default()
    };
    let endpoint = base_endpoint_builder(&options, vec![])
        .expect("endpoint builder")
        .bind()
        .await
        .expect("endpoint should bind");

    close_receive_endpoint(&endpoint)
        .await
        .expect("endpoint shutdown should complete");

    assert!(endpoint.is_closed());
}

#[test]
fn finalize_failed_receive_preserves_primary_error_when_cleanup_fails() {
    let err = finalize_failed_receive(
        anyhow::anyhow!("primary failure"),
        Err(anyhow::anyhow!("cleanup failure")),
    );
    assert!(err.to_string().contains("primary failure"));
}

#[test]
fn finalize_cleanup_returns_shutdown_error_even_if_cleanup_fails() {
    let shutdown_error = anyhow::anyhow!("shutdown failed");
    let cleanup_error = anyhow::anyhow!("cleanup failed");
    let err = finalize_cleanup(Err(shutdown_error), Err(cleanup_error))
        .expect_err("shutdown error should be preserved");
    assert!(err.to_string().contains("shutdown failed"));
}

#[test]
fn finalize_cleanup_succeeds_if_shutdown_succeeds() {
    finalize_cleanup(Ok(()), Err(anyhow::anyhow!("cleanup failed")))
        .expect("cleanup failures should not fail operation");
}

#[tokio::test]
async fn process_get_stream_errors_if_stream_ends_before_done() {
    let mut s = stream::empty::<GetProgressItem>();
    let mut reporter = receiver_reporter(None, 0);
    let err = process_get_stream_with_reporter(&mut s, 0, &mut reporter, None)
        .await
        .expect_err("stream ending early should fail");
    assert!(err.to_string().contains("ended before completion"));
}

#[tokio::test]
async fn receive_phase_timeout_reports_the_timed_out_operation() {
    let error = await_receive_phase(
        Some(Duration::from_millis(1)),
        TransferPhase::Connecting,
        "connect to test sender",
        std::future::pending::<anyhow::Result<()>>(),
    )
    .await
    .expect_err("pending operation should time out");

    assert!(
        error
            .to_string()
            .contains("connect to test sender timed out")
    );
    let details = classified_transfer_error(&error).expect("structured timeout");
    assert_eq!(details.code, TransferErrorCode::Timeout);
    assert_eq!(details.phase, TransferPhase::Connecting);
    assert!(details.retryable);
}

#[tokio::test]
async fn process_get_stream_times_out_after_idle_period() {
    let mut stream = stream::pending::<GetProgressItem>();
    let mut reporter = receiver_reporter(None, 0);

    let error = process_get_stream_with_reporter(
        &mut stream,
        0,
        &mut reporter,
        Some(Duration::from_millis(1)),
    )
    .await
    .expect_err("idle stream should time out");

    assert!(error.to_string().contains("without progress"));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn process_get_stream_resets_idle_timeout_after_progress() {
    let mut stream = Box::pin(stream::unfold(0u8, |step| async move {
        match step {
            0 => {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some((GetProgressItem::Progress(5), 1))
            }
            1 => {
                tokio::time::sleep(Duration::from_millis(5)).await;
                Some((GetProgressItem::Done(Default::default()), 2))
            }
            _ => None,
        }
    }));
    let mut reporter = receiver_reporter(None, 10);

    process_get_stream_with_reporter(
        &mut stream,
        0,
        &mut reporter,
        Some(Duration::from_millis(8)),
    )
    .await
    .expect("each stream item should reset the idle timer");
}