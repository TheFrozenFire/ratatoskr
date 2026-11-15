//! Header chain.

use alloy_primitives::B256;
use eyre::eyre;
use store_model::HeaderRecord;

use crate::{blocking, LocalStore};

#[allow(async_fn_in_trait)]
pub trait ChainStore: Send + Sync {
    async fn header_cursor(&self) -> eyre::Result<u64>;
    async fn header_parent(&self, next: u64) -> eyre::Result<B256>;
    async fn append_headers(&self, records: &[HeaderRecord]) -> eyre::Result<HeaderRecord>;
    async fn for_each_header(
        &self,
        from: u64,
        to: u64,
        visit: &mut dyn FnMut(&[HeaderRecord]) -> eyre::Result<()>,
    ) -> eyre::Result<u64>;
}

impl ChainStore for LocalStore {
    async fn header_cursor(&self) -> eyre::Result<u64> {
        let chain = self.chain_db()?;
        blocking(move || chain.cursor()).await
    }

    async fn header_parent(&self, next: u64) -> eyre::Result<B256> {
        let chain = self.chain_db()?;
        blocking(move || chain.parent_hash(next)).await
    }

    async fn append_headers(&self, records: &[HeaderRecord]) -> eyre::Result<HeaderRecord> {
        let chain = self.chain_db()?;
        let records = records.to_vec();
        blocking(move || {
            let next = chain.cursor()?;
            let parent = chain.parent_hash(next)?;
            chain.append(&records, next, parent)
        })
        .await
    }

    async fn for_each_header(
        &self,
        from: u64,
        to: u64,
        visit: &mut dyn FnMut(&[HeaderRecord]) -> eyre::Result<()>,
    ) -> eyre::Result<u64> {
        // `to` stays the caller's end. A callback that appends must not extend it.
        if from > to {
            return Err(eyre!("header range {from}..{to} is backwards"));
        }
        let mut seen = 0_u64;
        let mut at = from;
        let batch = self.caches.header_batch;
        while at < to {
            let end = at.saturating_add(batch).min(to);
            let chain = self.chain_db()?;
            let batch = blocking(move || chain.read_range(at, end)).await?;
            let rows = batch.len() as u64;
            if rows != end - at {
                seen += rows;
                return Err(short_header_range(seen, from, to));
            }
            visit(&batch)?;
            seen += rows;
            at = end;
        }
        if seen != to - from {
            return Err(short_header_range(seen, from, to));
        }
        Ok(seen)
    }
}

fn short_header_range(seen: u64, from: u64, to: u64) -> eyre::Report {
    let span = to - from;
    eyre!("header chain has {seen} rows for {span} blocks starting at {from}")
}
