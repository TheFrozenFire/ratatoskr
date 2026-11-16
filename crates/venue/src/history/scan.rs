//! Bloom scan over the stored header chain.

use std::time::Instant;

use alloy_primitives::Bloom;

use crate::logs::{job_filter, LogJob};
use crate::{ChainStore, DiscoveryStore};

pub(super) struct PendingJob {
    pub(super) job: LogJob,
    /// Next log block still to decode. Hits below this were already ingested.
    pub(super) at: u64,
    /// Next block already tested against the header bloom. Exclusive.
    pub(super) scan: u64,
    pub(super) filter: filter::LogFilter,
    pub(super) hits: Vec<u64>,
    pub(super) fresh: Vec<u64>,
}

async fn load_and_scan<S: ChainStore + DiscoveryStore>(
    store: &S,
    jobs: &[LogJob],
    stop: &tokio::sync::watch::Receiver<bool>,
    limits: &config::Venues,
) -> eyre::Result<BloomScan> {
    let mut pending = Vec::new();
    for job in jobs {
        let at = store
            .cursor(&job.name)
            .await?
            .unwrap_or(job.deploy_block)
            .max(job.deploy_block);
        let scan = store.hit_scan(&job.name).await?.unwrap_or(at).max(at);
        let hits = store.hits_from(&job.name, at, scan).await?;
        let after = store.cursor(&format!("{}:after", job.name)).await?;
        observability::info!(
            job = %job.name,
            at,
            scan,
            hits = hits.len(),
            ?after,
            "history cursor"
        );
        pending.push(PendingJob {
            filter: job_filter(job),
            job: LogJob {
                name: job.name.clone(),
                address: job.address,
                topics: job.topics.clone(),
                deploy_block: job.deploy_block,
            },
            at,
            scan,
            hits,
            fresh: Vec::new(),
        });
    }
    let end = store.header_cursor().await?;
    let start = pending
        .iter()
        .map(|job| job.scan)
        .min()
        .unwrap_or(end)
        .min(end);
    observability::info!(start, end, "scanning stored header blooms");
    let started = Instant::now();
    let mut scanned = 0_u64;
    let mut seen = 0_u64;
    let mut at = start;
    while at < end {
        if crate::hold_for_pause(stop).await {
            return Ok(BloomScan {
                end,
                jobs: pending,
                stopped: true,
            });
        }
        let to = at.saturating_add(limits.scan_flush).min(end);
        let window = store
            .for_each_header(at, to, &mut |batch| {
                for header in batch {
                    scanned += 1;
                    for job in &mut pending {
                        if header.number < job.scan {
                            continue;
                        }
                        if job.filter.bloom_maybe(&Bloom::from(header.logs_bloom)) {
                            job.hits.push(header.number);
                            job.fresh.push(header.number);
                        }
                    }
                }
                Ok(())
            })
            .await?;
        seen += window;
        flush_hits(store, &mut pending, to).await?;
        if to - at == limits.scan_flush {
            observability::info!(block = to.saturating_sub(1), scanned, "bloom scan");
        }
        at = to;
    }
    flush_hits(store, &mut pending, end).await?;
    let secs = started.elapsed().as_secs_f32().max(0.001);
    observability::info!(
        scanned = seen,
        secs,
        rate = (seen as f32 / secs) as u64,
        "bloom scan finished"
    );
    for job in &mut pending {
        job.hits.retain(|block| *block < end);
        observability::info!(
            job = %job.job.name,
            at = job.at,
            scan = job.scan,
            end,
            hits = job.hits.len(),
            "bloom hits"
        );
    }
    Ok(BloomScan {
        end,
        jobs: pending,
        stopped: false,
    })
}

pub struct BloomScan {
    pub(super) end: u64,
    pub(super) jobs: Vec<PendingJob>,
    pub(super) stopped: bool,
}

pub(crate) async fn scan_blooms<S: ChainStore + DiscoveryStore>(
    store: &S,
    jobs: &[LogJob],
    stop: &tokio::sync::watch::Receiver<bool>,
    limits: &config::Venues,
) -> eyre::Result<BloomScan> {
    load_and_scan(store, jobs, stop, limits).await
}

async fn flush_hits<S: DiscoveryStore>(
    store: &S,
    jobs: &mut [PendingJob],
    to: u64,
) -> eyre::Result<()> {
    for job in jobs.iter_mut() {
        if to <= job.scan {
            continue;
        }
        let mut write = Vec::new();
        let mut kept = Vec::new();
        for block in job.fresh.drain(..) {
            if block < to {
                write.push(block);
            } else {
                kept.push(block);
            }
        }
        job.fresh = kept;
        store.commit_hits(&job.job.name, to, &write).await?;
        job.scan = to;
        observability::info!(
            job = %job.job.name,
            scanned_to = to,
            hits = write.len(),
            "cached bloom hits"
        );
    }
    Ok(())
}
