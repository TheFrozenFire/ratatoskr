//! Receipt spans from the peer daemon, after the merge.

use std::collections::BTreeMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use eyre::WrapErr;

use super::scan::PendingJob;
use crate::catalog::Put;
use crate::logs::{Walk, MERGE_BLOCK};
use crate::{CorpusStore, DiscoveryStore};
use chain::decode::rows_from_log;
use journal::logs_in_block;
use peer::HistoryPeer;
use store_model::BlockRecord;

enum AskFail {
    Unavailable(String),
    Failed(String),
}

pub(super) async fn fetch_peer<S, P>(
    store: &S,
    peer: &P,
    job: &PendingJob,
    hits: &[u64],
    end: u64,
    stop: &tokio::sync::watch::Receiver<bool>,
    fetch: &super::ReceiptFetch<'_>,
) -> eyre::Result<Walk>
where
    S: CorpusStore + DiscoveryStore,
    P: HistoryPeer + Clone + 'static,
{
    let limits = fetch.limits;
    let archival = fetch.archival;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let spans = hit_spans(hits, limits.chunk_max);
    let mut next_span = 0_usize;
    let mut pending: BTreeMap<u64, tokio::task::JoinHandle<Result<SpanOut, AskFail>>> =
        BTreeMap::new();
    let mut rows = Vec::new();
    let mut fresh = Vec::new();
    let mut since_commit = 0_usize;
    let mut cursor = job.at.max(MERGE_BLOCK);
    let started = Instant::now();
    let mut fetched = 0_u64;
    let mut from_cache = 0_u64;
    loop {
        if crate::hold_for_pause(stop).await {
            abort(&mut pending);
            flush_bodies(store, &mut fresh, archival, now).await?;
            commit_rows(store, &job.job.name, cursor, &rows).await?;
            return Ok(Walk::Stopped {
                at: cursor,
                reason: "stop".into(),
            });
        }
        while pending.len() < limits.receipt_pipeline {
            let Some((start, count)) = spans.get(next_span).copied() else {
                break;
            };
            next_span += 1;
            let cached = match cached_prefix(store, start, count).await {
                Ok(cached) => cached,
                Err(fail) => {
                    abort(&mut pending);
                    flush_bodies(store, &mut fresh, archival, now).await?;
                    commit_rows(store, &job.job.name, cursor, &rows).await?;
                    return Ok(stop_ask(cursor, fail));
                }
            };
            let peer = peer.clone();
            let filter = job.filter.clone();
            let archival = *archival;
            pending.insert(
                start,
                tokio::spawn(async move {
                    finish_span(&peer, start, count, cached, &filter, archival, now).await
                }),
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
                flush_bodies(store, &mut fresh, archival, now).await?;
                commit_rows(store, &job.job.name, cursor, &rows).await?;
                return Ok(stop_ask(cursor, fail));
            }
            Err(err) => {
                abort(&mut pending);
                flush_bodies(store, &mut fresh, archival, now).await?;
                commit_rows(store, &job.job.name, cursor, &rows).await?;
                return Ok(Walk::Stopped {
                    at: cursor,
                    reason: format!("receipt task: {err}"),
                });
            }
        };
        if span.got == 0 {
            abort(&mut pending);
            flush_bodies(store, &mut fresh, archival, now).await?;
            commit_rows(store, &job.job.name, cursor, &rows).await?;
            return Ok(Walk::Stopped {
                at: cursor,
                reason: format!("chunk {start} returned no receipts"),
            });
        }
        from_cache += span.got.saturating_sub(span.fresh.len() as u64);
        fresh.extend(span.fresh);
        rows.extend(span.rows);
        fetched += span.got;
        cursor = start + span.got;
        since_commit += 1;
        if span.got < span.asked || since_commit >= limits.commit_spans {
            flush_bodies(store, &mut fresh, archival, now).await?;
            let wrote = commit_rows(store, &job.job.name, cursor, &rows).await?;
            rows.clear();
            since_commit = 0;
            let secs = started.elapsed().as_secs_f32().max(0.001);
            observability::info!(
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
    flush_bodies(store, &mut fresh, archival, now).await?;
    commit_rows(store, &job.job.name, end, &rows).await?;
    observability::info!(job = %job.job.name, at = end, fetched, cached = from_cache, "history caught up");
    Ok(Walk::Done)
}

/// Store receipt bodies that are still inside the archival window.
async fn flush_bodies<S: CorpusStore>(
    store: &S,
    fresh: &mut Vec<BlockRecord>,
    archival: &config::Archival,
    now: u64,
) -> eyre::Result<()> {
    if fresh.is_empty() {
        return Ok(());
    }
    fresh.retain(|record| archival.keeps_block(record.timestamp, now));
    if !fresh.is_empty() {
        store.put_blocks(fresh).await.wrap_err("cache receipts")?;
    }
    fresh.clear();
    Ok(())
}

async fn commit_rows<S: DiscoveryStore>(
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
    archival: config::Archival,
    now: u64,
) -> Result<SpanOut, AskFail> {
    let mut fresh = Vec::new();
    if (cached.len() as u64) < count {
        let at = start + cached.len() as u64;
        let need = count - cached.len() as u64;
        let decoded = request_chunk(peer, at, need).await?;
        let got = record_prefix(&decoded, at);
        fresh.extend(decoded.into_iter().take(got));
    }
    let mut rows = rows_of(&cached, filter, archival, now)?;
    rows.extend(rows_of(&fresh, filter, archival, now)?);
    Ok(SpanOut {
        asked: count,
        got: (cached.len() + fresh.len()) as u64,
        rows,
        fresh,
    })
}

async fn cached_prefix<S: CorpusStore>(
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
    archival: config::Archival,
    now: u64,
) -> Result<Vec<crate::VenueRow>, AskFail> {
    let mut rows = Vec::new();
    for record in records {
        let logs = logs_in_block(record)
            .map_err(|err| AskFail::Failed(format!("block {}: {err:#}", record.number)))?;
        let every_log = !archival.keeps_block(record.timestamp, now);
        for log in logs {
            if every_log || filter.matches(log.address, &log.topics) {
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

/// Ranges of at most `chunk_max` blocks that cover every hit.
///
/// Hits inside that window share one receipt request. A hit `chunk_max` or
/// more blocks later starts a new request, so an isolated hit stays one block.
pub(super) fn hit_spans(numbers: &[u64], chunk_max: u64) -> Vec<(u64, u64)> {
    let mut spans = Vec::new();
    let mut index = 0;
    while index < numbers.len() {
        let start = numbers[index];
        let mut end = start;
        index += 1;
        while index < numbers.len() && numbers[index] < start + chunk_max {
            end = numbers[index];
            index += 1;
        }
        spans.push((start, end - start + 1));
    }
    spans
}
