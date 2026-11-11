//! Receipt ranges from a [`ipc::HistoryPeer`] into the corpus.
//!
//! `write_records` is this path. A hole has no row batch: `write_hole` does
//! not move `cursor-*`, and `put_meta` does.

use crate::{RecordBatch, Sink, SinkReply, Source};
use eyre::WrapErr;
use ipc::{earliest_floor, max_head, serving, Earliest, HistoryPeer};
use model::BlockRecord;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use store::EntityStore;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{info, warn};

const CHUNK: u64 = 32;
/// SHiNode's default cap. Each peer can take two overlapping batches.
const PIPELINE: usize = 32;
const CHUNKS_PER_PEER: usize = 2;
/// Single-block misses with no data stored before a range is left for this run.
const PRUNE_STREAK: u32 = 3;
const GIB: u64 = 1024 * 1024 * 1024;

struct Range {
    name: &'static str,
    start: u64,
    end: u64,
    /// Stop this range after adding this many bytes. The recent window runs to
    /// its last block.
    byte_budget: Option<u64>,
}

enum PeerFloor {
    /// Every live peer advertised an earliest block.
    Known(u64),
    /// At least one peer speaks eth/68 and did not advertise a window.
    Unknown,
}

enum ChunkFate {
    Advance,
    Retry,
    LeaveRange,
}

pub struct ReceiptSource<S, P> {
    store: S,
    peer: P,
}

impl<S, P> ReceiptSource<S, P> {
    pub fn new(store: S, peer: P) -> Self {
        Self { store, peer }
    }
}

impl<S, P> Source for ReceiptSource<S, P>
where
    S: EntityStore + 'static,
    P: HistoryPeer + Clone + 'static,
{
    fn name(&self) -> &str {
        "receipts"
    }

    async fn run(
        self,
        sink: &mut dyn Sink,
        stop: tokio::sync::watch::Receiver<bool>,
    ) -> eyre::Result<()> {
        let head = wait_for_head(&self.peer, &stop).await?;
        info!(head, "peer head");
        for range in ranges(head) {
            if *stop.borrow() {
                break;
            }
            ingest_range(&self.store, &self.peer, sink, &range, &stop).await?;
        }
        Ok(())
    }
}

fn ranges(head: u64) -> Vec<Range> {
    let recent_end = head.saturating_sub(32);
    // About 55 days behind the peer head.
    let recent_start = recent_end.saturating_sub(399_999);
    vec![
        // UniswapV3Factory deployment, block 12369621, plus ~50k blocks.
        // Pre-merge. Kept only when a peer still advertises that height.
        Range {
            name: "univ3-launch",
            start: 12_369_621,
            end: 12_419_620,
            byte_budget: Some(2 * GIB),
        },
        // Uniswap v4 PoolManager deployment, block 21688329, plus ~100k blocks.
        Range {
            name: "univ4-launch",
            start: 21_688_329,
            end: 21_788_328,
            byte_budget: Some(3 * GIB),
        },
        Range {
            name: "recent",
            start: recent_start,
            end: recent_end,
            byte_budget: None,
        },
    ]
}

async fn ingest_range<S, P>(
    store: &S,
    peer: &P,
    sink: &mut dyn Sink,
    range: &Range,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<()>
where
    S: EntityStore,
    P: HistoryPeer + Clone + 'static,
{
    let cursor_key = format!("cursor-{}", range.name);
    let mut next = store.meta(&cursor_key).await?.unwrap_or(range.start);
    if next < range.start {
        next = range.start;
    }
    if next > range.end {
        info!(range = range.name, "range already stored");
        return Ok(());
    }
    // The first peer is often a recent-only node. Wait for one that still
    // serves this range before clipping or skipping it.
    let deadline = Instant::now() + Duration::from_secs(900);
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        match peer_floor(peer).await {
            Ok(PeerFloor::Unknown) => break,
            Ok(PeerFloor::Known(earliest)) if earliest <= next => break,
            Ok(PeerFloor::Known(earliest)) if Instant::now() >= deadline => {
                if earliest > range.end {
                    info!(
                        range = range.name,
                        earliest, "peers do not serve this range"
                    );
                    return Ok(());
                }
                info!(
                    range = range.name,
                    earliest, "range clipped to advertised earliest"
                );
                next = earliest;
                break;
            }
            Ok(PeerFloor::Known(earliest)) => {
                info!(
                    range = range.name,
                    earliest, "waiting for a peer that serves this range"
                );
                sleep(Duration::from_secs(2)).await;
            }
            Err(err) => {
                warn!(range = range.name, %err, "peer history window unknown");
                if Instant::now() >= deadline {
                    break;
                }
                sleep(Duration::from_secs(2)).await;
            }
        }
    }
    if next > range.end {
        info!(range = range.name, "range is past the advertised window");
        return Ok(());
    }
    info!(range = range.name, next, end = range.end, "range start");
    let mut single = false;
    let mut missing = 0u32;
    let mut soft = 0u32;
    let mut added_in_range = 0u64;
    let mut stored = store.meta("bytes").await?.unwrap_or(0);
    while next <= range.end {
        if *stop.borrow() {
            return Ok(());
        }
        if range
            .byte_budget
            .is_some_and(|budget| added_in_range >= budget)
        {
            info!(
                range = range.name,
                added_in_range, "range byte budget reached"
            );
            break;
        }
        let depth = if single {
            1
        } else {
            serving_peers(peer, next)
                .await
                .saturating_mul(CHUNKS_PER_PEER)
                .clamp(1, PIPELINE)
        };
        let mut plan = Vec::new();
        let mut issue = next;
        for _ in 0..depth {
            if issue > range.end {
                break;
            }
            let width = if single {
                1
            } else {
                (range.end - issue + 1).min(CHUNK)
            };
            plan.push((issue, width));
            issue = issue.saturating_add(width);
            if single {
                break;
            }
        }
        if plan.is_empty() {
            break;
        }
        let mut join = JoinSet::new();
        for (start, width) in &plan {
            let start = *start;
            let width = *width;
            let peer = peer.clone();
            join.spawn(async move {
                let started = Instant::now();
                let result = peer
                    .chunk(start, width)
                    .await
                    .map_err(|err| format!("{err:#}"));
                (start, width, result, started)
            });
        }
        let mut done = BTreeMap::new();
        while let Some(joined) = join.join_next().await {
            let (start, width, result, started) = joined.wrap_err("chunk task")?;
            done.insert(start, (width, result, started));
        }
        let mut stop_batch = false;
        for (start, _) in &plan {
            if *start != next || stop_batch {
                break;
            }
            let (width, result, started) = done
                .remove(start)
                .ok_or_else(|| eyre::eyre!("planned chunk {start} did not finish"))?;
            let fate = apply_chunk(
                store,
                sink,
                &cursor_key,
                range,
                &mut next,
                &mut stored,
                &mut added_in_range,
                &mut single,
                &mut missing,
                &mut soft,
                width,
                result,
                started.elapsed().as_secs_f32(),
            )
            .await?;
            match fate {
                ChunkFate::Advance => {}
                ChunkFate::Retry => {
                    let pause = if soft > 4 { 10 } else { 2 };
                    sleep(Duration::from_secs(pause)).await;
                    stop_batch = true;
                }
                ChunkFate::LeaveRange => return Ok(()),
            }
        }
    }
    store.put_meta(&cursor_key, next).await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn apply_chunk<S: EntityStore>(
    store: &S,
    sink: &mut dyn Sink,
    cursor_key: &str,
    range: &Range,
    next: &mut u64,
    stored: &mut u64,
    added_in_range: &mut u64,
    single: &mut bool,
    missing: &mut u32,
    soft: &mut u32,
    width: u64,
    result: Result<Vec<BlockRecord>, String>,
    secs: f32,
) -> eyre::Result<ChunkFate> {
    match result {
        Ok(records) if !records.is_empty() => {
            let records: Arc<[BlockRecord]> = Arc::from(records);
            let before = store.meta("bytes").await?.unwrap_or(0);
            match sink
                .push(RecordBatch::Blocks {
                    cursor_key: Some(Arc::from(cursor_key)),
                    records: Arc::clone(&records),
                })
                .await?
            {
                SinkReply::Applied => {}
                SinkReply::Rejected { reason } => return Err(eyre::eyre!(reason)),
            }
            let after = store.meta("bytes").await?.unwrap_or(0);
            let added = after.saturating_sub(before);
            *stored = stored.saturating_add(added);
            *added_in_range = added_in_range.saturating_add(added);
            let last = records.last().map(|record| record.number).unwrap_or(*next);
            info!(
                range = range.name,
                from = records[0].number,
                to = last,
                blocks = records.len(),
                added,
                stored = *stored,
                secs,
                "chunk stored"
            );
            *next = last.saturating_add(1);
            *single = false;
            *missing = 0;
            *soft = 0;
            Ok(ChunkFate::Advance)
        }
        Ok(_) => {
            *missing = missing.saturating_add(1);
            warn!(range = range.name, next = *next, missing, "empty chunk");
            *single = true;
            if *missing >= PRUNE_STREAK && *added_in_range == 0 {
                warn!(range = range.name, next = *next, "range looks unavailable");
                return Ok(ChunkFate::LeaveRange);
            }
            Ok(ChunkFate::Retry)
        }
        Err(err) if is_wait(&err) => {
            warn!(range = range.name, next = *next, %err, "waiting for a peer");
            Ok(ChunkFate::Retry)
        }
        Err(err) if err.contains("no peer serves") => {
            warn!(range = range.name, next = *next, %err, "range looks unavailable");
            Ok(ChunkFate::LeaveRange)
        }
        Err(err) if err.contains("no header") => {
            *missing = missing.saturating_add(1);
            warn!(
                range = range.name,
                next = *next,
                width,
                missing,
                %err,
                "chunk failed"
            );
            if width > 1 {
                *single = true;
                return Ok(ChunkFate::Retry);
            }
            if *added_in_range == 0 && *missing >= PRUNE_STREAK {
                warn!(range = range.name, next = *next, "range looks unavailable");
                return Ok(ChunkFate::LeaveRange);
            }
            if *missing >= 24 {
                warn!(range = range.name, next = *next, "range looks unavailable");
                store.put_meta(cursor_key, *next).await?;
                return Ok(ChunkFate::LeaveRange);
            }
            if *added_in_range > 0 {
                store.write_hole(*next).await?;
                *next = next.saturating_add(1);
            }
            *single = true;
            Ok(ChunkFate::Retry)
        }
        Err(err) if is_soft(&err) => {
            *soft = soft.saturating_add(1);
            warn!(
                range = range.name,
                next = *next,
                width,
                soft,
                %err,
                "chunk failed"
            );
            if *added_in_range == 0 && *soft >= 12 {
                warn!(
                    range = range.name,
                    next = *next,
                    "leaving range after repeated transport errors"
                );
                return Ok(ChunkFate::LeaveRange);
            }
            Ok(ChunkFate::Retry)
        }
        Err(err) => {
            warn!(
                range = range.name,
                next = *next,
                width,
                %err,
                "chunk failed"
            );
            if width > 1 {
                *single = true;
                return Ok(ChunkFate::Retry);
            }
            store.write_hole(*next).await?;
            *next = next.saturating_add(1);
            *single = false;
            Ok(ChunkFate::Retry)
        }
    }
}

fn is_wait(err: &str) -> bool {
    err.contains("no peers") || err.contains("busy") || err.contains("peer process")
}

fn is_soft(err: &str) -> bool {
    err.contains("timed out")
        || err.contains("disconnected")
        || err.contains("peer closed")
        || err.contains("Connection reset")
        || err.contains("broken pipe")
        || err.contains("eth message")
        || err.contains("dropped the request")
        || err.contains("stream")
}

async fn peer_floor(peer: &impl HistoryPeer) -> eyre::Result<PeerFloor> {
    match earliest_floor(&peer.status().await?)? {
        Earliest::Open => Ok(PeerFloor::Unknown),
        Earliest::Known(block) => Ok(PeerFloor::Known(block)),
    }
}

async fn serving_peers(peer: &impl HistoryPeer, start: u64) -> usize {
    match peer.status().await {
        Ok(sessions) => serving(&sessions, start),
        Err(_) => 1,
    }
}

async fn wait_for_head(
    peer: &impl HistoryPeer,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<u64> {
    let started = Instant::now();
    loop {
        if *stop.borrow() {
            return Err(eyre::eyre!("stopped while waiting for a peer head"));
        }
        match peer.status().await {
            Ok(sessions) => {
                if let Some(head) = max_head(&sessions) {
                    return Ok(head);
                }
            }
            Err(err) => {
                if started.elapsed() > Duration::from_secs(15) {
                    warn!(%err, "still waiting for the peer process");
                }
            }
        }
        if started.elapsed() > Duration::from_secs(300) {
            return Err(eyre::eyre!("no peer head after 5 minutes"));
        }
        sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::{is_soft, is_wait, ReceiptSource};
    use crate::{Source, StoreSink};
    use alloy_primitives::B256;
    use ipc::{HistoryPeer, SessionInfo};
    use model::{BlockRecord, HeaderRecord};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use store::{EntityStore, LocalStore};

    #[test]
    fn wait_errors_are_not_block_failures() {
        assert!(is_wait("no peers connected"));
        assert!(is_wait("peers are busy"));
        assert!(!is_wait("peer has no header for block 12369621"));
        assert!(!is_wait("no peer serves block 12369621"));
    }

    #[test]
    fn transport_errors_stay_retryable() {
        assert!(is_soft("eth response timed out"));
        assert!(is_soft("peer dropped the request"));
        assert!(!is_soft("receipt root mismatch for block 10"));
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-receipt-{}-{}",
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

    fn block(number: u64) -> BlockRecord {
        BlockRecord {
            number,
            hash: B256::from([1_u8; 32]),
            parent_hash: B256::from([2_u8; 32]),
            timestamp: 1,
            receipts_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_zstd: vec![1, 2, 3],
        }
    }

    #[derive(Clone)]
    struct ScriptPeer {
        chunks: Arc<AtomicUsize>,
        /// When false, a request at block 1 fails with `no header`.
        serve_one: bool,
    }

    impl HistoryPeer for ScriptPeer {
        async fn status(&self) -> eyre::Result<Vec<SessionInfo>> {
            Ok(vec![SessionInfo {
                label: "a".into(),
                eth: 68,
                head: Some(33),
                earliest: Some(0),
            }])
        }

        async fn headers(&self, _start: u64, _count: u64) -> eyre::Result<Vec<HeaderRecord>> {
            Ok(Vec::new())
        }

        async fn chunk(&self, start: u64, _count: u64) -> eyre::Result<Vec<BlockRecord>> {
            self.chunks.fetch_add(1, Ordering::Relaxed);
            if start == 0 {
                return Ok(vec![block(0)]);
            }
            if start == 1 && !self.serve_one {
                return Err(eyre::eyre!("peer has no header for block {start}"));
            }
            Ok(vec![block(start)])
        }
    }

    fn stop() -> tokio::sync::watch::Receiver<bool> {
        tokio::sync::watch::channel(false).1
    }

    async fn preset_old_ranges(store: &LocalStore) {
        store
            .put_meta("cursor-univ3-launch", 12_419_621)
            .await
            .unwrap();
        store
            .put_meta("cursor-univ4-launch", 21_788_329)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_hole_skip_survives_restart_only_because_put_meta_ran() {
        let dir = TempDir::new();
        let path = dir.0.join("corpus.redb");
        let store = LocalStore::corpus(&path).unwrap();
        preset_old_ranges(&store).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let peer = ScriptPeer {
            chunks: Arc::clone(&calls),
            serve_one: false,
        };
        let mut sink = StoreSink::new(store.clone());
        ReceiptSource::new(store.clone(), peer)
            .run(&mut sink, stop())
            .await
            .unwrap();
        assert!(calls.load(Ordering::Relaxed) >= 1);
        assert_eq!(store.meta("cursor-recent").await.unwrap(), Some(2));
        assert!(store.meta("holes").await.unwrap().unwrap() >= 1);
        calls.store(0, Ordering::Relaxed);
        let peer = ScriptPeer {
            chunks: Arc::clone(&calls),
            serve_one: false,
        };
        let mut sink = StoreSink::new(store.clone());
        ReceiptSource::new(store.clone(), peer)
            .run(&mut sink, stop())
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 0);

        let bare = LocalStore::corpus(&dir.0.join("bare.redb")).unwrap();
        preset_old_ranges(&bare).await;
        bare.write_records("cursor-recent", &[block(0)])
            .await
            .unwrap();
        bare.write_hole(1).await.unwrap();
        assert_eq!(bare.meta("cursor-recent").await.unwrap(), Some(1));
        let calls = Arc::new(AtomicUsize::new(0));
        let peer = ScriptPeer {
            chunks: Arc::clone(&calls),
            serve_one: true,
        };
        let mut sink = StoreSink::new(bare.clone());
        ReceiptSource::new(bare.clone(), peer)
            .run(&mut sink, stop())
            .await
            .unwrap();
        assert!(calls.load(Ordering::Relaxed) >= 1);
    }
}
