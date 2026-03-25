use crate::core::session::TransferSession;

#[async_trait::async_trait]
pub trait TransferFlow {
    type Output;

    async fn run(self, session: &mut TransferSession) -> anyhow::Result<Self::Output>;
}