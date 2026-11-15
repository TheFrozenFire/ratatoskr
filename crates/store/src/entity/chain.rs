//! Header chain.

use alloy_primitives::B256;
use api::{Op, Reply, READ_CAP};
use eyre::eyre;
use model::HeaderRecord;

use crate::remote::RemoteStore;
use crate::{blocking, LocalStore};

const HEADER_BATCH: u64 = 8192;

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
        while at < to {
            let end = at.saturating_add(HEADER_BATCH).min(to);
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

impl ChainStore for RemoteStore {
    async fn header_cursor(&self) -> eyre::Result<u64> {
        match self.request(Op::HeaderCursor).await? {
            Reply::HeaderCursor(cursor) => Ok(cursor),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn header_parent(&self, next: u64) -> eyre::Result<B256> {
        match self.request(Op::HeaderParent { next }).await? {
            Reply::Parent(hash) => Ok(hash),
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn append_headers(&self, records: &[HeaderRecord]) -> eyre::Result<HeaderRecord> {
        let last = records
            .last()
            .cloned()
            .ok_or_else(|| eyre!("empty header batch"))?;
        match self
            .request(Op::AppendHeaders {
                records: records.to_vec(),
            })
            .await?
        {
            Reply::Appended { next, hash } => {
                if next != last.number.saturating_add(1) || hash != last.hash {
                    return Err(eyre!("header append reply does not match the batch"));
                }
                Ok(last)
            }
            _ => Err(eyre!("unexpected reply")),
        }
    }

    async fn for_each_header(
        &self,
        from: u64,
        to: u64,
        visit: &mut dyn FnMut(&[HeaderRecord]) -> eyre::Result<()>,
    ) -> eyre::Result<u64> {
        if from > to {
            return Err(eyre!("header range {from}..{to} is backwards"));
        }
        let mut seen = 0_u64;
        let mut at = from;
        while at < to {
            let limit = (to - at).min(READ_CAP);
            let batch = match self.request(Op::ReadHeaders { from: at, limit }).await? {
                Reply::Headers(batch) => batch,
                _ => return Err(eyre!("unexpected reply")),
            };
            let rows = batch.len() as u64;
            if rows != limit {
                seen += rows;
                return Err(short_header_range(seen, from, to));
            }
            visit(&batch)?;
            seen += rows;
            at = at.saturating_add(rows);
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
