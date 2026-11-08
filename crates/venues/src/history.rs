//! Event history for factories that keep no index.
//!
//! The local header chain already holds every logs bloom through the tip.
//! Bloom hits are written to the venue catalog as they are found. Receipts
//! for those hits come from era1 before the merge and from the peer daemon
//! after it. Each downloaded payload is stored before the cursor moves, so a
//! retry reads redb instead of the network.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use alloy_primitives::Bloom;
use chain::HeaderChain;
use corpus::{logs_in_block, Corpus};
use eyre::WrapErr;
use ipc::{body_call, socket_path};
use model::{decode_block_records, BlockRecord};
use serde_json::{json, Value};

use crate::catalog::{Put, ReceiptBody};
use crate::decode::rows_from_log;
use crate::logs::{job_filter, LogJob, Walk, MERGE_BLOCK};
use crate::Catalog;

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

pub async fn ingest_all(catalog: &Catalog, jobs: &[LogJob]) -> eyre::Result<Vec<String>> {
    let mut pending = Vec::new();
    for job in jobs {
        let at = catalog
            .cursor(&job.name)?
            .unwrap_or(job.deploy_block)
            .max(job.deploy_block);
        let scan = catalog.hit_scan(&job.name)?.unwrap_or(at).max(at);
        let hits = catalog.hits_from(&job.name, at, scan)?;
        let after = catalog.cursor(&format!("{}:after", job.name))?;
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
    let chain = HeaderChain::open(&HeaderChain::default_path()).wrap_err("header chain")?;
    let end = chain.cursor()?;
    let start = pending
        .iter()
        .map(|job| job.scan)
        .min()
        .unwrap_or(end)
        .min(end);
    tracing::info!(start, end, "scanning stored header blooms");
    let started = Instant::now();
    let mut scanned = 0_u64;
    let seen = chain.visit(start, end, |header| {
        scanned += 1;
        let through = header.number.saturating_add(1);
        for job in &mut pending {
            if header.number < job.scan {
                continue;
            }
            if job.filter.bloom_maybe(&Bloom::from(header.logs_bloom)) {
                job.hits.push(header.number);
                job.fresh.push(header.number);
            }
        }
        if scanned.is_multiple_of(SCAN_FLUSH) {
            tracing::info!(block = header.number, scanned, "bloom scan");
            flush_hits(catalog, &mut pending, through)?;
        }
        Ok(())
    })?;
    flush_hits(catalog, &mut pending, end)?;
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
    drop(chain);
    tracing::info!("released header chain");

    let corpus = Arc::new(open_receipt_cache()?);
    let socket = socket_path().wrap_err("peer socket")?;
    let mut notes = Vec::new();
    for job in pending {
        if job.at >= end {
            tracing::info!(job = %job.job.name, at = job.at, end, "history caught up");
            continue;
        }
        match fetch_job(catalog, &corpus, &job, &socket, end).await {
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

fn flush_hits(catalog: &Catalog, jobs: &mut [PendingJob], to: u64) -> eyre::Result<()> {
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
        catalog.commit_hits(&job.job.name, to, &write)?;
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

fn open_receipt_cache() -> eyre::Result<Corpus> {
    let path = Corpus::default_path();
    if path.exists() {
        Corpus::open(&path).wrap_err("receipt cache")
    } else {
        Corpus::create(&path).wrap_err("receipt cache")
    }
}

async fn fetch_job(
    catalog: &Catalog,
    corpus: &Arc<Corpus>,
    job: &PendingJob,
    socket: &Path,
    end: u64,
) -> eyre::Result<Walk> {
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
        if let Some(stopped) = fetch_era(catalog, job, &pre).await? {
            return Ok(stopped);
        }
        catalog.commit_chunk(&job.job.name, MERGE_BLOCK.min(end), &[], Put::Merge)?;
    }
    if end <= MERGE_BLOCK {
        return Ok(Walk::Done);
    }
    fetch_peer(catalog, corpus, job, socket, &post, end).await
}

async fn fetch_era(
    catalog: &Catalog,
    job: &PendingJob,
    hits: &[u64],
) -> eyre::Result<Option<Walk>> {
    if hits.is_empty() {
        return Ok(None);
    }
    let mut http: Option<era::EraHttp> = None;
    let mut done_through = job.at;
    for chunk in hits.chunks(ERA_BATCH) {
        let started = Instant::now();
        let cached = catalog.read_bodies(chunk)?;
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
            if http.is_none() {
                http = Some(era::EraHttp::new(ERA_INFLIGHT)?);
            }
            let client = http.as_ref().expect("http client");
            let fetched = era::receipts_for_blocks(
                client,
                MIRROR,
                "mainnet",
                &missing,
                &job.filter,
                ERA_INFLIGHT,
            )
            .await?;
            let mut store = Vec::with_capacity(fetched.len());
            for block in fetched {
                store.push((
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
            catalog.put_bodies(&store)?;
        }
        logs.sort_by_key(|log| (log.block_number, log.tx_index, log.log_index));
        let rows = logs.iter().flat_map(rows_from_log).collect::<Vec<_>>();
        let next = chunk
            .last()
            .copied()
            .unwrap_or(done_through)
            .saturating_add(1);
        let wrote = catalog.commit_chunk(&job.job.name, next, &rows, Put::Merge)?;
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

async fn fetch_peer(
    catalog: &Catalog,
    corpus: &Arc<Corpus>,
    job: &PendingJob,
    socket: &Path,
    hits: &[u64],
    end: u64,
) -> eyre::Result<Walk> {
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
        while pending.len() < RECEIPT_PIPELINE {
            let Some((start, count)) = spans.get(next_span).copied() else {
                break;
            };
            next_span += 1;
            let socket = socket.to_path_buf();
            let filter = job.filter.clone();
            let corpus = Arc::clone(corpus);
            pending.insert(
                start,
                tokio::spawn(
                    async move { fetch_span(&socket, &corpus, start, count, &filter).await },
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
                commit_rows(catalog, &job.job.name, cursor, &rows)?;
                return Ok(stop_ask(cursor, fail));
            }
            Err(err) => {
                abort(&mut pending);
                commit_rows(catalog, &job.job.name, cursor, &rows)?;
                return Ok(Walk::Stopped {
                    at: cursor,
                    reason: format!("receipt task: {err}"),
                });
            }
        };
        if span.got == 0 {
            abort(&mut pending);
            commit_rows(catalog, &job.job.name, cursor, &rows)?;
            return Ok(Walk::Stopped {
                at: cursor,
                reason: format!("chunk {start} returned no receipts"),
            });
        }
        if let Err(err) = corpus.put_blocks(&span.fresh) {
            abort(&mut pending);
            commit_rows(catalog, &job.job.name, cursor, &rows)?;
            return Err(err.wrap_err("cache receipts"));
        }
        from_cache += span.got.saturating_sub(span.fresh.len() as u64);
        rows.extend(span.rows);
        fetched += span.got;
        cursor = start + span.got;
        since_commit += 1;
        if span.got < span.asked || since_commit >= COMMIT_SPANS {
            let wrote = commit_rows(catalog, &job.job.name, cursor, &rows)?;
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
    commit_rows(catalog, &job.job.name, end, &rows)?;
    tracing::info!(job = %job.job.name, at = end, fetched, cached = from_cache, "history caught up");
    Ok(Walk::Done)
}

fn commit_rows(
    catalog: &Catalog,
    name: &str,
    next: u64,
    rows: &[crate::VenueRow],
) -> eyre::Result<u64> {
    catalog.commit_chunk(name, next, rows, Put::Merge)
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

async fn fetch_span(
    socket: &Path,
    corpus: &Corpus,
    start: u64,
    count: u64,
    filter: &filter::LogFilter,
) -> Result<SpanOut, AskFail> {
    let cached = cached_prefix(corpus, start, count)?;
    let mut fresh = Vec::new();
    if (cached.len() as u64) < count {
        let at = start + cached.len() as u64;
        let need = count - cached.len() as u64;
        let body = request_body(
            socket,
            json!({"op": "chunk", "start": at, "count": need}),
            "chunk",
        )
        .await?;
        let decoded = decode_block_records(&body)
            .map_err(|err| AskFail::Failed(format!("chunk {at}: {err}")))?;
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

fn cached_prefix(corpus: &Corpus, start: u64, count: u64) -> Result<Vec<BlockRecord>, AskFail> {
    let mut found = Vec::new();
    for offset in 0..count {
        let number = start + offset;
        let record = corpus
            .read_block(number)
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

async fn request_body(socket: &Path, request: Value, what: &str) -> Result<Vec<u8>, AskFail> {
    let mut last = String::new();
    for attempt in 0..12 {
        match body_call(socket, &request, what).await {
            Ok(body) => return Ok(body),
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
}
