//! Pair-conduit jobs.

use alloy_primitives::Address;
use store_model::{ConduitFail, ConduitJob};

use crate::{blocking, LocalStore};

#[allow(async_fn_in_trait)]
pub trait ConduitStore: Send + Sync {
    /// Queue conduit jobs for ordered pairs whose stored merge is missing or stale.
    async fn enqueue_conduit_fill(&self) -> eyre::Result<u64>;

    async fn claim_conduits(
        &self,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<ConduitJob>>;

    async fn finish_conduits(
        &self,
        done: &[(Address, Address)],
        again: &[(Address, Address)],
        failed: &[ConduitFail],
    ) -> eyre::Result<()>;

    /// Merge one ordered pair. The bool asks for another pass.
    async fn build_conduit(&self, token_in: Address, token_out: Address) -> eyre::Result<bool>;
}

impl ConduitStore for LocalStore {
    async fn enqueue_conduit_fill(&self) -> eyre::Result<u64> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.enqueue_conduit_fill()).await
    }

    async fn claim_conduits(
        &self,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<ConduitJob>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.claim_conduits(limit, now, lease_for)).await
    }

    async fn finish_conduits(
        &self,
        done: &[(Address, Address)],
        again: &[(Address, Address)],
        failed: &[ConduitFail],
    ) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let done = done.to_vec();
        let again = again.to_vec();
        let failed = failed.to_vec();
        blocking(move || catalog.finish_conduits(&done, &again, &failed)).await
    }

    async fn build_conduit(&self, token_in: Address, token_out: Address) -> eyre::Result<bool> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.build_conduit(token_in, token_out)).await
    }
}
