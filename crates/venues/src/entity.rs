//! Store operations venue and ingest code can name.
//!
//! The implementation lives in `store`. The trait is here so those crates can
//! take it without a dependency cycle.

use alloy_primitives::B256;
use model::{BlockRecord, HeaderRecord};

use crate::state::{
    CurveRecord, ImmutablePut, ImmutableRow, MutableStored, MutableWrite, PoolState, TickPage,
    WatchHit, WorkFail, WorkJob,
};
use crate::{Put, ReceiptBody, VenueRow};

#[allow(async_fn_in_trait)]
pub trait EntityStore: Send + Sync {
    async fn header_cursor(&self) -> eyre::Result<u64>;
    async fn header_parent(&self, next: u64) -> eyre::Result<B256>;
    async fn append_headers(&self, records: &[HeaderRecord]) -> eyre::Result<HeaderRecord>;
    async fn for_each_header(
        &self,
        from: u64,
        to: u64,
        visit: &mut dyn FnMut(&[HeaderRecord]) -> eyre::Result<()>,
    ) -> eyre::Result<u64>;

    async fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>>;
    async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64>;
    async fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64>;
    async fn write_hole(&self, number: u64) -> eyre::Result<()>;
    async fn meta(&self, key: &str) -> eyre::Result<Option<u64>>;
    async fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()>;
    async fn for_each_block(
        &self,
        visit: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()>;

    async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>>;
    async fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64>;
    async fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>>;
    async fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>>;
    async fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()>;
    async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>>;
    async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()>;

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
