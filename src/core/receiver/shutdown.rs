use crate::core::shutdown::with_shutdown_deadline;
use iroh::Endpoint;
use std::path::Path;

/// Close the receive endpoint before its temporary store is finalized.
pub(super) async fn close_receive_endpoint(endpoint: &Endpoint) -> anyhow::Result<()> {
    if endpoint.is_closed() {
        return Ok(());
    }
    with_shutdown_deadline("receive endpoint shutdown", async {
        endpoint.close().await;
        Ok(())
    })
    .await
}

pub(super) async fn remove_temp_receive_dir(path: &Path) -> anyhow::Result<()> {
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}
