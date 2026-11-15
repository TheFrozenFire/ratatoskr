//! Receipt bodies from era1, before the merge.

use std::time::Instant;

use alloy_primitives::Bloom;

use super::scan::PendingJob;
use crate::catalog::{Put, ReceiptBody};
use crate::logs::Walk;
use crate::DiscoveryStore;
use chain::decode::rows_from_log;

pub(super) trait EraFetch {
    fn fetch<'a>(
        &'a mut self,
        missing: &'a [u64],
        filter: &'a filter::LogFilter,
    ) -> impl std::future::Future<Output = eyre::Result<Vec<era::EraBlock>>> + Send + 'a;
}

pub(super) struct LiveEra {
    pub(super) http: Option<era::EraHttp>,
    pub(super) mirror: String,
    pub(super) inflight: usize,
}

impl EraFetch for LiveEra {
    async fn fetch(
        &mut self,
        missing: &[u64],
        filter: &filter::LogFilter,
    ) -> eyre::Result<Vec<era::EraBlock>> {
        if self.http.is_none() {
            self.http = Some(era::EraHttp::new(self.inflight)?);
        }
        let client = self.http.as_ref().expect("http client");
        era::receipts_for_blocks(
            client,
            &self.mirror,
            "mainnet",
            missing,
            filter,
            self.inflight,
        )
        .await
    }
}

pub(super) async fn fetch_era<S, E>(
    store: &S,
    era: &mut E,
    job: &PendingJob,
    hits: &[u64],
    stop: &tokio::sync::watch::Receiver<bool>,
    limits: &config::Venues,
) -> eyre::Result<Option<Walk>>
where
    S: DiscoveryStore,
    E: EraFetch,
{
    if hits.is_empty() {
        return Ok(None);
    }
    let mut done_through = job.at;
    for chunk in hits.chunks(limits.era_batch) {
        if crate::hold_for_pause(stop).await {
            return Ok(Some(Walk::Stopped {
                at: done_through,
                reason: "stop".into(),
            }));
        }
        let started = Instant::now();
        let cached = store.read_bodies(chunk).await?;
        let mut missing = Vec::new();
        let mut logs = Vec::new();
        let mut from_cache = 0_usize;
        for (block, body) in chunk.iter().copied().zip(cached) {
            let Some(body) = body else {
                missing.push(block);
                continue;
            };
            let header = era::HeaderRow {
                number: block,
                hash: body.hash,
                parent: body.parent_hash,
                timestamp: body.timestamp,
                receipts_root: body.receipts_root,
                logs_bloom: Bloom::default(),
            };
            match era::logs_from_snappy(&header, &body.payload, &job.filter) {
                Ok(found) => {
                    from_cache += 1;
                    logs.extend(found);
                }
                Err(err) => {
                    observability::warn!(block, error = %err, "cached era body rejected");
                    missing.push(block);
                }
            }
        }
        if !missing.is_empty() {
            let fetched = era.fetch(&missing, &job.filter).await?;
            let mut store_bodies = Vec::with_capacity(fetched.len());
            for block in fetched {
                store_bodies.push((
                    block.header.number,
                    ReceiptBody {
                        hash: block.header.hash,
                        parent_hash: block.header.parent,
                        timestamp: block.header.timestamp,
                        receipts_root: block.header.receipts_root,
                        payload: block.receipts_snappy,
                    },
                ));
                logs.extend(block.logs);
            }
            store.put_bodies(&store_bodies).await?;
        }
        logs.sort_by_key(|log| (log.block_number, log.tx_index, log.log_index));
        let rows = logs.iter().flat_map(rows_from_log).collect::<Vec<_>>();
        let next = chunk
            .last()
            .copied()
            .unwrap_or(done_through)
            .saturating_add(1);
        let wrote = store
            .commit_chunk(&job.job.name, next, &rows, Put::Merge)
            .await?;
        let secs = started.elapsed().as_secs_f32().max(0.001);
        observability::info!(
            job = %job.job.name,
            from = done_through,
            to = next - 1,
            hits = chunk.len(),
            cached = from_cache,
            fetched = missing.len(),
            rows = rows.len(),
            wrote,
            rate = (chunk.len() as f32 / secs) as u64,
            secs,
            "era hits"
        );
        done_through = next;
    }
    Ok(None)
}
