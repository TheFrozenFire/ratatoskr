//! Event history for factories that keep no index.
//!
//! The local header chain already holds every logs bloom through the tip.
//! Bloom hits are written to the venue catalog as they are found. Receipts
//! for those hits come from era1 before the merge and from the peer daemon
//! after it. Each downloaded payload is stored before the cursor moves, so a
//! retry reads redb instead of the network.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use alloy_primitives::Bloom;
use corpus::logs_in_block;
use eyre::{eyre, WrapErr};
use ipc::{socket_path, HistoryPeer};
use model::BlockRecord;

use crate::catalog::{Put, ReceiptBody};
use crate::decode::rows_from_log;
use crate::logs::{job_filter, LogJob, Walk, MERGE_BLOCK};
use crate::EntityStore;

const CHUNK_MAX: u64 = 64;
const RECEIPT_PIPELINE: usize = 8;
const ERA_INFLIGHT: usize = 32;
const ERA_BATCH: usize = 128;
const COMMIT_SPANS: usize = 32;
const SCAN_FLUSH: u64 = 1_000_000;
const MIRROR: &str = "https://data.ethpandaops.io/era1/mainnet/";

#[derive(Debug)]
enum AskFail {
    Unavailable(String),
    Failed(String),
}

struct PendingJob {
    job: LogJob,
    /// Next log block still to decode. Hits below this were already ingested.
    at: u64,
    /// Next block already tested against the header bloom. Exclusive.
    scan: u64,
    filter: filter::LogFilter,
    hits: Vec<u64>,
    fresh: Vec<u64>,
}

trait EraFetch {
    fn fetch<'a>(
        &'a mut self,
        missing: &'a [u64],
        filter: &'a filter::LogFilter,
    ) -> impl std::future::Future<Output = eyre::Result<Vec<era::EraBlock>>> + Send + 'a;
}

struct LiveEra {
    http: Option<era::EraHttp>,
}

impl EraFetch for LiveEra {
    async fn fetch(
        &mut self,
        missing: &[u64],
        filter: &filter::LogFilter,
    ) -> eyre::Result<Vec<era::EraBlock>> {
        if self.http.is_none() {
            self.http = Some(era::EraHttp::new(ERA_INFLIGHT)?);
        }
        let client = self.http.as_ref().expect("http client");
        era::receipts_for_blocks(client, MIRROR, "mainnet", missing, filter, ERA_INFLIGHT).await
    }
}

pub async fn ingest_all<S, P>(
    store: &S,
    peer: Option<&P>,
    jobs: &[LogJob],
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<Vec<String>>
where
    S: EntityStore,
    P: HistoryPeer + Clone + 'static,
{
    let scan = scan_blooms(store, jobs, stop).await?;
    fetch_receipts(store, peer, scan, stop).await
}

async fn load_and_scan<S: EntityStore>(
    store: &S,
    jobs: &[LogJob],
    stop: &tokio::sync::watch::Receiver<bool>,
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
        tracing::info!(
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
    tracing::info!(start, end, "scanning stored header blooms");
    let started = Instant::now();
    let mut scanned = 0_u64;
    let mut seen = 0_u64;
    let mut at = start;
    while at < end {
        if *stop.borrow() {
            return Ok(BloomScan {
                end,
                jobs: pending,
                stopped: true,
            });
        }
        let to = at.saturating_add(SCAN_FLUSH).min(end);
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
        if to - at == SCAN_FLUSH {
            tracing::info!(block = to.saturating_sub(1), scanned, "bloom scan");
        }
        at = to;
    }
    flush_hits(store, &mut pending, end).await?;
    let secs = started.elapsed().as_secs_f32().max(0.001);
    tracing::info!(
        scanned = seen,
        secs,
        rate = (seen as f32 / secs) as u64,
        "bloom scan finished"
    );
    for job in &mut pending {
        job.hits.retain(|block| *block < end);
        tracing::info!(
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
    end: u64,
    jobs: Vec<PendingJob>,
    stopped: bool,
}

pub(crate) async fn scan_blooms<S: EntityStore>(
    store: &S,
    jobs: &[LogJob],
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<BloomScan> {
    load_and_scan(store, jobs, stop).await
}

pub(crate) async fn fetch_receipts<S, P>(
    store: &S,
    peer: Option<&P>,
    scan: BloomScan,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<Vec<String>>
where
    S: EntityStore,
    P: HistoryPeer + Clone + 'static,
{
    if scan.stopped {
        return Ok(Vec::new());
    }
    let end = scan.end;
    let pending = scan.jobs;
    let Some(peer) = peer else {
        socket_path().wrap_err("peer socket")?;
        return Err(eyre!("peer socket"));
    };
    let mut era = LiveEra { http: None };
    let mut notes = Vec::new();
    for job in pending {
        if *stop.borrow() {
            break;
        }
        if job.at >= end {
            tracing::info!(job = %job.job.name, at = job.at, end, "history caught up");
            continue;
        }
        match fetch_job(store, peer, &mut era, &job, end, stop).await {
            Ok(Walk::Done) => tracing::info!(job = %job.job.name, "history finished"),
            Ok(Walk::Stopped { at, reason }) => {
                let note = format!("{} stopped at {at}: {reason}", job.job.name);
                tracing::warn!(%note);
                notes.push(note);
            }
            Err(err) => {
                let note = format!("{}: {err:#}", job.job.name);
                tracing::error!(%note);
                notes.push(note);
            }
        }
    }
    Ok(notes)
}

async fn flush_hits<S: EntityStore>(
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
        tracing::info!(
            job = %job.job.name,
            scanned_to = to,
            hits = write.len(),
            "cached bloom hits"
        );
    }
    Ok(())
}

async fn fetch_job<S, P, E>(
    store: &S,
    peer: &P,
    era: &mut E,
    job: &PendingJob,
    end: u64,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<Walk>
where
    S: EntityStore,
    P: HistoryPeer + Clone + 'static,
    E: EraFetch,
{
    let pre: Vec<u64> = job
        .hits
        .iter()
        .copied()
        .filter(|block| *block < MERGE_BLOCK)
        .collect();
    let post: Vec<u64> = job
        .hits
        .iter()
        .copied()
        .filter(|block| *block >= MERGE_BLOCK)
        .collect();
    if job.at < MERGE_BLOCK {
        if let Some(stopped) = fetch_era(store, era, job, &pre, stop).await? {
            return Ok(stopped);
        }
        store
            .commit_chunk(&job.job.name, MERGE_BLOCK.min(end), &[], Put::Merge)
            .await?;
    }
    if end <= MERGE_BLOCK {
        return Ok(Walk::Done);
    }
    fetch_peer(store, peer, job, &post, end, stop).await
}

async fn fetch_era<S, E>(
    store: &S,
    era: &mut E,
    job: &PendingJob,
    hits: &[u64],
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<Option<Walk>>
where
    S: EntityStore,
    E: EraFetch,
{
    if hits.is_empty() {
        return Ok(None);
    }
    let mut done_through = job.at;
    for chunk in hits.chunks(ERA_BATCH) {
        if *stop.borrow() {
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
                    tracing::warn!(block, error = %err, "cached era body rejected");
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
        tracing::info!(
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

async fn fetch_peer<S, P>(
    store: &S,
    peer: &P,
    job: &PendingJob,
    hits: &[u64],
    end: u64,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<Walk>
where
    S: EntityStore,
    P: HistoryPeer + Clone + 'static,
{
    let spans = hit_spans(hits);
    let mut next_span = 0_usize;
    let mut pending: BTreeMap<u64, tokio::task::JoinHandle<Result<SpanOut, AskFail>>> =
        BTreeMap::new();
    let mut rows = Vec::new();
    let mut since_commit = 0_usize;
    let mut cursor = job.at.max(MERGE_BLOCK);
    let started = Instant::now();
    let mut fetched = 0_u64;
    let mut from_cache = 0_u64;
    loop {
        if *stop.borrow() {
            abort(&mut pending);
            commit_rows(store, &job.job.name, cursor, &rows).await?;
            return Ok(Walk::Stopped {
                at: cursor,
                reason: "stop".into(),
            });
        }
        while pending.len() < RECEIPT_PIPELINE {
            let Some((start, count)) = spans.get(next_span).copied() else {
                break;
            };
            next_span += 1;
            let cached = match cached_prefix(store, start, count).await {
                Ok(cached) => cached,
                Err(fail) => {
                    abort(&mut pending);
                    commit_rows(store, &job.job.name, cursor, &rows).await?;
                    return Ok(stop_ask(cursor, fail));
                }
            };
            let peer = peer.clone();
            let filter = job.filter.clone();
            pending.insert(
                start,
                tokio::spawn(
                    async move { finish_span(&peer, start, count, cached, &filter).await },
                ),
            );
        }
        let Some(start) = pending.keys().next().copied() else {
            break;
        };
        let handle = pending.remove(&start).expect("span was just keyed");
        let span = match handle.await {
            Ok(Ok(span)) => span,
            Ok(Err(fail)) => {
                abort(&mut pending);
                commit_rows(store, &job.job.name, cursor, &rows).await?;
                return Ok(stop_ask(cursor, fail));
            }
            Err(err) => {
                abort(&mut pending);
                commit_rows(store, &job.job.name, cursor, &rows).await?;
                return Ok(Walk::Stopped {
                    at: cursor,
                    reason: format!("receipt task: {err}"),
                });
            }
        };
        if span.got == 0 {
            abort(&mut pending);
            commit_rows(store, &job.job.name, cursor, &rows).await?;
            return Ok(Walk::Stopped {
                at: cursor,
                reason: format!("chunk {start} returned no receipts"),
            });
        }
        if let Err(err) = store.put_blocks(&span.fresh).await {
            abort(&mut pending);
            commit_rows(store, &job.job.name, cursor, &rows).await?;
            return Err(err.wrap_err("cache receipts"));
        }
        from_cache += span.got.saturating_sub(span.fresh.len() as u64);
        rows.extend(span.rows);
        fetched += span.got;
        cursor = start + span.got;
        since_commit += 1;
        if span.got < span.asked || since_commit >= COMMIT_SPANS {
            let wrote = commit_rows(store, &job.job.name, cursor, &rows).await?;
            rows.clear();
            since_commit = 0;
            let secs = started.elapsed().as_secs_f32().max(0.001);
            tracing::info!(
                job = %job.job.name,
                at = cursor,
                fetched,
                cached = from_cache,
                wrote,
                rate = (fetched as f32 / secs) as u64,
                "receipts"
            );
        }
        if span.got < span.asked {
            abort(&mut pending);
            return Ok(Walk::Stopped {
                at: cursor,
                reason: format!(
                    "chunk {start} returned {} of {} blocks",
                    span.got, span.asked
                ),
            });
        }
    }
    commit_rows(store, &job.job.name, end, &rows).await?;
    tracing::info!(job = %job.job.name, at = end, fetched, cached = from_cache, "history caught up");
    Ok(Walk::Done)
}

async fn commit_rows<S: EntityStore>(
    store: &S,
    name: &str,
    next: u64,
    rows: &[crate::VenueRow],
) -> eyre::Result<u64> {
    store.commit_chunk(name, next, rows, Put::Merge).await
}

fn abort(pending: &mut BTreeMap<u64, tokio::task::JoinHandle<Result<SpanOut, AskFail>>>) {
    while let Some((_, handle)) = pending.pop_first() {
        handle.abort();
    }
}

fn stop_ask(at: u64, fail: AskFail) -> Walk {
    let reason = match fail {
        AskFail::Unavailable(reason) | AskFail::Failed(reason) => reason,
    };
    Walk::Stopped { at, reason }
}

struct SpanOut {
    asked: u64,
    got: u64,
    rows: Vec<crate::VenueRow>,
    /// Receipts that were not already in the corpus.
    fresh: Vec<BlockRecord>,
}

async fn finish_span<P: HistoryPeer>(
    peer: &P,
    start: u64,
    count: u64,
    cached: Vec<BlockRecord>,
    filter: &filter::LogFilter,
) -> Result<SpanOut, AskFail> {
    let mut fresh = Vec::new();
    if (cached.len() as u64) < count {
        let at = start + cached.len() as u64;
        let need = count - cached.len() as u64;
        let decoded = request_chunk(peer, at, need).await?;
        let got = record_prefix(&decoded, at);
        fresh.extend(decoded.into_iter().take(got));
    }
    let mut rows = rows_of(&cached, filter)?;
    rows.extend(rows_of(&fresh, filter)?);
    Ok(SpanOut {
        asked: count,
        got: (cached.len() + fresh.len()) as u64,
        rows,
        fresh,
    })
}

async fn cached_prefix<S: EntityStore>(
    store: &S,
    start: u64,
    count: u64,
) -> Result<Vec<BlockRecord>, AskFail> {
    let mut found = Vec::new();
    for offset in 0..count {
        let number = start + offset;
        let record = store
            .read_block(number)
            .await
            .map_err(|err| AskFail::Failed(format!("corpus block {number}: {err:#}")))?;
        match record {
            Some(record) if record.number == number && !record.receipts_zstd.is_empty() => {
                found.push(record);
            }
            _ => break,
        }
    }
    Ok(found)
}

fn rows_of(
    records: &[BlockRecord],
    filter: &filter::LogFilter,
) -> Result<Vec<crate::VenueRow>, AskFail> {
    let mut rows = Vec::new();
    for record in records {
        let logs = logs_in_block(record)
            .map_err(|err| AskFail::Failed(format!("block {}: {err:#}", record.number)))?;
        for log in logs {
            if filter.matches(log.address, &log.topics) {
                rows.extend(rows_from_log(&log));
            }
        }
    }
    Ok(rows)
}

async fn request_chunk<P: HistoryPeer>(
    peer: &P,
    start: u64,
    count: u64,
) -> Result<Vec<BlockRecord>, AskFail> {
    let mut last = String::new();
    for attempt in 0..12 {
        match peer.chunk(start, count).await {
            Ok(records) => return Ok(records),
            Err(err) => {
                let msg = err.to_string();
                if msg.contains("no peer serves block") {
                    return Err(AskFail::Unavailable(msg));
                }
                let retry = msg.contains("busy")
                    || msg.contains("no peers")
                    || msg.contains("dropped")
                    || msg.contains("connect")
                    || msg.contains("closed the socket")
                    || msg.contains("peer process");
                last = msg;
                if !retry && attempt >= 2 {
                    return Err(AskFail::Failed(last));
                }
                if attempt + 1 == 12 {
                    return Err(AskFail::Failed(last));
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
    Err(AskFail::Failed(last))
}

fn record_prefix(records: &[BlockRecord], start: u64) -> usize {
    records
        .iter()
        .enumerate()
        .take_while(|(offset, record)| {
            record.number == start + *offset as u64 && !record.receipts_zstd.is_empty()
        })
        .count()
}

/// Ranges of at most [`CHUNK_MAX`] blocks that cover every hit.
///
/// Hits inside that window share one receipt request. A hit 64 or more
/// blocks later starts a new request, so an isolated hit stays one block.
fn hit_spans(numbers: &[u64]) -> Vec<(u64, u64)> {
    let mut spans = Vec::new();
    let mut index = 0;
    while index < numbers.len() {
        let start = numbers[index];
        let mut end = start;
        index += 1;
        while index < numbers.len() && numbers[index] < start + CHUNK_MAX {
            end = numbers[index];
            index += 1;
        }
        spans.push((start, end - start + 1));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{b256, Address, B256};
    use corpus::Corpus;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Files {
        corpus: Corpus,
        catalog: crate::Catalog,
    }

    impl EntityStore for Files {
        async fn header_cursor(&self) -> eyre::Result<u64> {
            Err(eyre::eyre!("unused"))
        }
        async fn header_parent(&self, _: u64) -> eyre::Result<alloy_primitives::B256> {
            Err(eyre::eyre!("unused"))
        }
        async fn append_headers(
            &self,
            _: &[model::HeaderRecord],
        ) -> eyre::Result<model::HeaderRecord> {
            Err(eyre::eyre!("unused"))
        }
        async fn for_each_header(
            &self,
            _: u64,
            _: u64,
            _: &mut dyn FnMut(&[model::HeaderRecord]) -> eyre::Result<()>,
        ) -> eyre::Result<u64> {
            Err(eyre::eyre!("unused"))
        }
        async fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>> {
            self.corpus.read_block(number)
        }
        async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64> {
            self.corpus.put_blocks(records)
        }
        async fn write_records(&self, _: &str, _: &[BlockRecord]) -> eyre::Result<u64> {
            Err(eyre::eyre!("unused"))
        }
        async fn write_hole(&self, _: u64) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn meta(&self, key: &str) -> eyre::Result<Option<u64>> {
            self.corpus.meta(key)
        }
        async fn put_meta(&self, _: &str, _: u64) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn for_each_block(
            &self,
            _: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
        ) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
            self.catalog.cursor(job)
        }
        async fn commit_chunk(
            &self,
            job: &str,
            next: u64,
            rows: &[crate::VenueRow],
            put: Put,
        ) -> eyre::Result<u64> {
            self.catalog.commit_chunk(job, next, rows, put)
        }
        async fn hit_scan(&self, _: &str) -> eyre::Result<Option<u64>> {
            Err(eyre::eyre!("unused"))
        }
        async fn hits_from(&self, _: &str, _: u64, _: u64) -> eyre::Result<Vec<u64>> {
            Err(eyre::eyre!("unused"))
        }
        async fn commit_hits(&self, _: &str, _: u64, _: &[u64]) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
            self.catalog.read_bodies(numbers)
        }
        async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
            self.catalog.put_bodies(bodies)
        }
        async fn read_venue(&self, _: u8, _: B256) -> eyre::Result<Option<crate::VenueRow>> {
            Err(eyre::eyre!("unused"))
        }
        async fn reconcile(&self, _: u64) -> eyre::Result<u64> {
            Err(eyre::eyre!("unused"))
        }
        async fn claim_work(
            &self,
            _: u8,
            _: u32,
            _: u64,
            _: u64,
        ) -> eyre::Result<Vec<crate::WorkJob>> {
            Err(eyre::eyre!("unused"))
        }
        async fn finish_work(
            &self,
            _: &[(u8, u8, B256)],
            _: &[crate::WorkFail],
        ) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn enqueue_work(&self, _: &[crate::WorkJob], _: u64) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn put_immutable(
            &self,
            _: &[crate::ImmutableRow],
        ) -> eyre::Result<Vec<crate::ImmutablePut>> {
            Err(eyre::eyre!("unused"))
        }
        async fn put_mutable(&self, _: &crate::MutableWrite) -> eyre::Result<crate::MutableStored> {
            Err(eyre::eyre!("unused"))
        }
        async fn put_tick_pages(&self, _: &[crate::TickPage]) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn put_curve(&self, _: &crate::CurveRecord) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn read_state(&self, _: u8, _: B256) -> eyre::Result<crate::PoolState> {
            Err(eyre::eyre!("unused"))
        }
        async fn apply_watch(&self, _: u64, _: &[crate::WatchHit]) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
        async fn watch_through(&self) -> eyre::Result<Option<u64>> {
            Err(eyre::eyre!("unused"))
        }
    }

    #[test]
    fn hit_spans_split_a_long_run_and_a_gap() {
        let hits: Vec<u64> = (10..75).collect();
        assert_eq!(hit_spans(&hits), vec![(10, 64), (74, 1)]);
        assert_eq!(hit_spans(&[1, 2, 4]), vec![(1, 4)]);
        assert_eq!(hit_spans(&[1, 63]), vec![(1, 63)]);
        assert_eq!(hit_spans(&[1, 64, 65]), vec![(1, 64), (65, 1)]);
        assert_eq!(hit_spans(&[1, 65]), vec![(1, 1), (65, 1)]);
        assert!(hit_spans(&[]).is_empty());
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-history-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn empty_root() -> B256 {
        b256!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421")
    }

    fn zstd_block(number: u64) -> BlockRecord {
        let raw = corpus::encode_receipt_bundle(&[]);
        let receipts_zstd = zstd::stream::encode_all(raw.as_slice(), 0).unwrap();
        BlockRecord {
            number,
            hash: B256::from([1_u8; 32]),
            parent_hash: B256::from([2_u8; 32]),
            timestamp: 1,
            receipts_root: empty_root(),
            transactions_root: B256::ZERO,
            receipts_zstd,
        }
    }

    fn pending(name: &str, at: u64) -> PendingJob {
        let job = LogJob {
            name: name.into(),
            address: Address::ZERO,
            topics: Vec::new(),
            deploy_block: at,
        };
        PendingJob {
            filter: job_filter(&job),
            job,
            at,
            scan: at,
            hits: vec![at],
            fresh: Vec::new(),
        }
    }

    #[derive(Clone)]
    struct FakePeer {
        calls: Arc<AtomicUsize>,
    }

    impl HistoryPeer for FakePeer {
        async fn status(&self) -> eyre::Result<Vec<ipc::SessionInfo>> {
            Ok(Vec::new())
        }

        async fn headers(
            &self,
            _start: u64,
            _count: u64,
        ) -> eyre::Result<Vec<model::HeaderRecord>> {
            Ok(Vec::new())
        }

        async fn chunk(&self, start: u64, _count: u64) -> eyre::Result<Vec<BlockRecord>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![zstd_block(start)])
        }
    }

    struct FakeEra {
        calls: Arc<AtomicUsize>,
    }

    impl EraFetch for FakeEra {
        async fn fetch(
            &mut self,
            missing: &[u64],
            _filter: &filter::LogFilter,
        ) -> eyre::Result<Vec<era::EraBlock>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(missing
                .iter()
                .copied()
                .map(|number| era::EraBlock {
                    header: era::HeaderRow {
                        number,
                        hash: B256::from([1_u8; 32]),
                        parent: B256::from([2_u8; 32]),
                        timestamp: 1,
                        receipts_root: empty_root(),
                        logs_bloom: Bloom::default(),
                    },
                    receipts_snappy: Vec::new(),
                    logs: Vec::new(),
                })
                .collect())
        }
    }

    fn stop() -> tokio::sync::watch::Receiver<bool> {
        tokio::sync::watch::channel(false).1
    }

    #[tokio::test]
    async fn cached_payload_is_not_fetched_again_when_the_cursor_is_uncommitted() {
        let dir = TempDir::new();
        let store = Files {
            corpus: Corpus::create(&dir.0.join("corpus.redb")).unwrap(),
            catalog: crate::Catalog::open(&dir.0.join("venues.redb")).unwrap(),
        };
        let era_calls = Arc::new(AtomicUsize::new(0));
        let peer_calls = Arc::new(AtomicUsize::new(0));
        let mut era = FakeEra {
            calls: Arc::clone(&era_calls),
        };
        let peer = FakePeer {
            calls: Arc::clone(&peer_calls),
        };
        let cached_era = 10_u64;
        store
            .put_bodies(&[(
                cached_era,
                ReceiptBody {
                    hash: B256::from([1_u8; 32]),
                    parent_hash: B256::from([2_u8; 32]),
                    timestamp: 1,
                    receipts_root: empty_root(),
                    payload: Vec::new(),
                },
            )])
            .await
            .unwrap();
        let cached_block = MERGE_BLOCK;
        store.put_blocks(&[zstd_block(cached_block)]).await.unwrap();
        let era_job = pending("era-cached", cached_era);
        let peer_job = pending("peer-cached", cached_block);
        assert!(store.cursor(&era_job.job.name).await.unwrap().is_none());
        assert!(store.cursor(&peer_job.job.name).await.unwrap().is_none());

        fetch_era(&store, &mut era, &era_job, &[cached_era], &stop())
            .await
            .unwrap();
        fetch_peer(
            &store,
            &peer,
            &peer_job,
            &[cached_block],
            cached_block + 1,
            &stop(),
        )
        .await
        .unwrap();
        assert_eq!(era_calls.load(Ordering::Relaxed), 0);
        assert_eq!(peer_calls.load(Ordering::Relaxed), 0);

        let missed = 11_u64;
        fetch_era(
            &store,
            &mut era,
            &pending("era-miss", missed),
            &[missed],
            &stop(),
        )
        .await
        .unwrap();
        fetch_peer(
            &store,
            &peer,
            &pending("peer-miss", cached_block + 2),
            &[cached_block + 2],
            cached_block + 3,
            &stop(),
        )
        .await
        .unwrap();
        assert_eq!(era_calls.load(Ordering::Relaxed), 1);
        assert_eq!(peer_calls.load(Ordering::Relaxed), 1);
    }
}
