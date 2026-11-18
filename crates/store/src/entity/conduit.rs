//! Pair-conduit persistence.

use crate::service::{ConduitBody, ConduitMember, CurvePage, MemberTarget, PendingMember};
use alloy_primitives::{Address, B256};
use store_model::{ConduitFail, ConduitJob};

use crate::{blocking, LocalStore};

#[allow(async_fn_in_trait)]
pub trait ConduitStore: Send + Sync {
    async fn page_curves(&self, after: Option<(u8, B256)>, limit: usize)
        -> eyre::Result<CurvePage>;

    async fn member_fill_done(&self) -> eyre::Result<bool>;

    async fn pending_member_curves(&self) -> eyre::Result<Vec<PendingMember>>;

    async fn mark_member_fill_done(&self) -> eyre::Result<()>;

    async fn enqueue_members(&self, targets: &[MemberTarget]) -> eyre::Result<u64>;

    async fn conduit_members(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Vec<ConduitMember>>;

    async fn store_conduit(&self, body: &ConduitBody) -> eyre::Result<bool>;

    async fn apply_retarget(
        &self,
        kind: u8,
        id: B256,
        old_pairs: &[(Address, Address)],
        new_pairs: &[(Address, Address)],
        generation: Option<u64>,
    ) -> eyre::Result<()>;

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
}

impl ConduitStore for LocalStore {
    async fn page_curves(
        &self,
        after: Option<(u8, B256)>,
        limit: usize,
    ) -> eyre::Result<CurvePage> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.page_curves(after, limit)).await
    }

    async fn member_fill_done(&self) -> eyre::Result<bool> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.member_fill_done()).await
    }

    async fn pending_member_curves(&self) -> eyre::Result<Vec<PendingMember>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.pending_member_curves()).await
    }

    async fn mark_member_fill_done(&self) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.mark_member_fill_done()).await
    }

    async fn enqueue_members(&self, targets: &[MemberTarget]) -> eyre::Result<u64> {
        let catalog = self.catalog_db()?;
        let targets = targets.to_vec();
        blocking(move || catalog.enqueue_members(&targets)).await
    }

    async fn conduit_members(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Vec<ConduitMember>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.conduit_members(token_in, token_out)).await
    }

    async fn store_conduit(&self, body: &ConduitBody) -> eyre::Result<bool> {
        let catalog = self.catalog_db()?;
        let body = body.clone();
        blocking(move || catalog.store_conduit(&body)).await
    }

    async fn apply_retarget(
        &self,
        kind: u8,
        id: B256,
        old_pairs: &[(Address, Address)],
        new_pairs: &[(Address, Address)],
        generation: Option<u64>,
    ) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let old_pairs = old_pairs.to_vec();
        let new_pairs = new_pairs.to_vec();
        blocking(move || catalog.apply_retarget(kind, id, &old_pairs, &new_pairs, generation)).await
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
}
