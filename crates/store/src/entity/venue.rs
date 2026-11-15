//! Venue identity, work, pool state, and the watch.

use alloy_primitives::B256;
use eyre::eyre;
use store_model::{
    CurveRecord, ImmutablePut, ImmutableRow, MutableStored, MutableWrite, PoolState, QuoteRecord,
    TickPage, VenueRow, WatchHit, WorkFail, WorkJob,
};

use crate::remote::{empty_state, RemoteStore};
use crate::wire::{StateCall, StateReply};
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
    async fn put_quote(&self, quote: &QuoteRecord) -> eyre::Result<()>;
    async fn put_quotes(&self, quotes: &[QuoteRecord]) -> eyre::Result<()>;
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

    async fn put_quote(&self, quote: &QuoteRecord) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let quote = quote.clone();
        blocking(move || catalog.put_quote(&quote)).await
    }

    async fn put_quotes(&self, quotes: &[QuoteRecord]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let quotes = quotes.to_vec();
        blocking(move || catalog.put_quotes(&quotes)).await
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

impl VenueStore for RemoteStore {
    async fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        match self.state(StateCall::ReadVenue { kind, id }).await? {
            StateReply::Venue(row) => Ok(row),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn reconcile(&self, now: u64) -> eyre::Result<u64> {
        match self.state(StateCall::Reconcile { now }).await? {
            StateReply::Wrote(n) => Ok(n),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn claim_work(
        &self,
        stage: u8,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<WorkJob>> {
        match self
            .state(StateCall::ClaimWork {
                stage,
                limit,
                now,
                lease_for,
            })
            .await?
        {
            StateReply::Jobs(jobs) => Ok(jobs),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn finish_work(&self, done: &[(u8, u8, B256)], failed: &[WorkFail]) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::FinishWork {
                done: done.to_vec(),
                failed: failed.to_vec(),
            })
            .await?,
        )
    }

    async fn enqueue_work(&self, jobs: &[WorkJob], now: u64) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::EnqueueWork {
                jobs: jobs.to_vec(),
                now,
            })
            .await?,
        )
    }

    async fn put_immutable(&self, rows: &[ImmutableRow]) -> eyre::Result<Vec<ImmutablePut>> {
        match self
            .state(StateCall::PutImmutable {
                rows: rows.to_vec(),
            })
            .await?
        {
            StateReply::Puts(puts) => Ok(puts),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn put_mutable(&self, row: &MutableWrite) -> eyre::Result<MutableStored> {
        match self
            .state(StateCall::PutMutable { row: row.clone() })
            .await?
        {
            StateReply::Stored(stored) => Ok(stored),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn put_tick_pages(&self, pages: &[TickPage]) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::PutTickPages {
                pages: pages.to_vec(),
            })
            .await?,
        )
    }

    async fn put_curve(&self, curve: &CurveRecord) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::PutCurve {
                curve: curve.clone(),
            })
            .await?,
        )
    }

    async fn put_quote(&self, quote: &QuoteRecord) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::PutQuote {
                quote: quote.clone(),
            })
            .await?,
        )
    }

    /// The pool-state body carries one quote.
    async fn put_quotes(&self, quotes: &[QuoteRecord]) -> eyre::Result<()> {
        for quote in quotes {
            self.put_quote(quote).await?;
        }
        Ok(())
    }

    async fn read_state(&self, kind: u8, id: B256) -> eyre::Result<PoolState> {
        match self.state(StateCall::ReadState { kind, id }).await? {
            StateReply::State(state) => Ok(*state),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn apply_watch(&self, through: u64, hits: &[WatchHit]) -> eyre::Result<()> {
        empty_state(
            self.state(StateCall::ApplyWatch {
                through,
                hits: hits.to_vec(),
            })
            .await?,
        )
    }

    async fn watch_through(&self) -> eyre::Result<Option<u64>> {
        match self.state(StateCall::WatchThrough).await? {
            StateReply::Cursor(cursor) => Ok(cursor),
            _ => Err(eyre!("unexpected reply")),
        }
    }
}
