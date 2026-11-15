//! Venue identity, work, pool state, and the watch.

use alloy_primitives::B256;
use store_model::{
    CurveRecord, ImmutablePut, ImmutableRow, MutableStored, MutableWrite, PoolState, TickPage,
    VenueRow, WatchHit, WorkFail, WorkJob,
};

use crate::{blocking, LocalStore};

#[allow(async_fn_in_trait)]
pub trait VenueStore: Send + Sync {
    async fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>>;
    async fn reconcile(&self, now: u64) -> eyre::Result<u64>;
    async fn claim_work(
        &self,
        stage: u8,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<WorkJob>>;
    async fn finish_work(&self, done: &[(u8, u8, B256)], failed: &[WorkFail]) -> eyre::Result<()>;
    async fn enqueue_work(&self, jobs: &[WorkJob], now: u64) -> eyre::Result<()>;
    async fn put_immutable(&self, rows: &[ImmutableRow]) -> eyre::Result<Vec<ImmutablePut>>;
    async fn put_mutable(&self, row: &MutableWrite) -> eyre::Result<MutableStored>;
    async fn put_tick_pages(&self, pages: &[TickPage]) -> eyre::Result<()>;
    async fn put_curve(&self, curve: &CurveRecord) -> eyre::Result<()>;
    async fn read_state(&self, kind: u8, id: B256) -> eyre::Result<PoolState>;
    async fn apply_watch(&self, through: u64, hits: &[WatchHit]) -> eyre::Result<()>;
    async fn watch_through(&self) -> eyre::Result<Option<u64>>;
}

impl VenueStore for LocalStore {
    async fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.read_venue(kind, id)).await
    }

    async fn reconcile(&self, now: u64) -> eyre::Result<u64> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.reconcile(now)).await
    }

    async fn claim_work(
        &self,
        stage: u8,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<WorkJob>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.claim_work(stage, limit, now, lease_for)).await
    }

    async fn finish_work(&self, done: &[(u8, u8, B256)], failed: &[WorkFail]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let done = done.to_vec();
        let failed = failed.to_vec();
        blocking(move || catalog.finish_work(&done, &failed)).await
    }

    async fn enqueue_work(&self, jobs: &[WorkJob], now: u64) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let jobs = jobs.to_vec();
        blocking(move || catalog.enqueue_work(&jobs, now)).await
    }

    async fn put_immutable(&self, rows: &[ImmutableRow]) -> eyre::Result<Vec<ImmutablePut>> {
        let catalog = self.catalog_db()?;
        let rows = rows.to_vec();
        blocking(move || catalog.put_immutable(&rows)).await
    }

    async fn put_mutable(&self, row: &MutableWrite) -> eyre::Result<MutableStored> {
        let catalog = self.catalog_db()?;
        let row = row.clone();
        blocking(move || catalog.put_mutable(&row)).await
    }

    async fn put_tick_pages(&self, pages: &[TickPage]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let pages = pages.to_vec();
        blocking(move || catalog.put_tick_pages(&pages)).await
    }

    async fn put_curve(&self, curve: &CurveRecord) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let curve = curve.clone();
        blocking(move || catalog.put_curve(&curve)).await
    }

    async fn read_state(&self, kind: u8, id: B256) -> eyre::Result<PoolState> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.read_state(kind, id)).await
    }

    async fn apply_watch(&self, through: u64, hits: &[WatchHit]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let hits = hits.to_vec();
        blocking(move || catalog.apply_watch(through, &hits)).await
    }

    async fn watch_through(&self) -> eyre::Result<Option<u64>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.watch_through()).await
    }
}
