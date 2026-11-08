//! Header sync. Chunk, tip lag, and in-flight cap are the existing loop.

use eyre::WrapErr;
use ipc::{max_head, serving, HistoryPeer};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use store::EntityStore;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::{RecordBatch, Sink, SinkReply, Source};

/// One `GetBlockHeaders` round. Peers cap this near 1024.
const CHUNK: u64 = 512;
const PIPELINE: usize = 16;
const CHUNKS_PER_PEER: usize = 2;
/// Stay behind the peer's head so a reorg does not rewrite the chain tip.
const TIP_LAG: u64 = 32;

/// Fetches header batches and submits only the next cursor, in order.
///
/// `Rejected` retries that same `next`. Stop finishes the push already in
/// flight and does not start another fetch.
pub struct HeaderSource<P, S> {
    peer: P,
    store: S,
}

impl<P, S> HeaderSource<P, S> {
    pub fn new(peer: P, store: S) -> Self {
        Self { peer, store }
    }
}

impl<P, S> Source for HeaderSource<P, S>
where
    P: HistoryPeer + Clone + 'static,
    S: EntityStore,
{
    fn name(&self) -> &str {
        "headers"
    }

    async fn run(self, sink: &mut dyn Sink, mut stop: watch::Receiver<bool>) -> eyre::Result<()> {
        let mut next = self.store.header_cursor().await?;
        // Target `headers` matches the binary filter when `RUST_LOG` is unset.
        info!(target: "headers", next, "header chain open");
        loop {
            if *stop.borrow() {
                return Ok(());
            }
            let head = match self.peer.status().await {
                Ok(sessions) => max_head(&sessions),
                Err(_) => None,
            };
            let Some(head) = head else {
                warn!(target: "headers", "waiting for a peer head");
                if wait_or_stop(&mut stop, 2).await {
                    return Ok(());
                }
                continue;
            };
            let tip = head.saturating_sub(TIP_LAG);
            if next > tip {
                info!(target: "headers", next, head, "caught up");
                if wait_or_stop(&mut stop, 15).await {
                    return Ok(());
                }
                continue;
            }
            let depth = match self.peer.status().await {
                Ok(sessions) => serving(&sessions, next),
                Err(_) => 1,
            }
            .saturating_mul(CHUNKS_PER_PEER)
            .clamp(1, PIPELINE);
            let mut plan = Vec::new();
            let mut issue = next;
            for _ in 0..depth {
                if issue > tip {
                    break;
                }
                let width = (tip - issue + 1).min(CHUNK);
                plan.push((issue, width));
                issue += width;
            }
            if plan.is_empty() {
                if wait_or_stop(&mut stop, 2).await {
                    return Ok(());
                }
                continue;
            }
            if *stop.borrow() {
                return Ok(());
            }
            let mut join = JoinSet::new();
            for (start, width) in &plan {
                let start = *start;
                let width = *width;
                let peer = self.peer.clone();
                join.spawn(async move {
                    let started = Instant::now();
                    let result = peer.headers(start, width).await;
                    (start, result, started)
                });
            }
            let mut done = BTreeMap::new();
            while let Some(joined) = join.join_next().await {
                let (start, result, started) = joined.wrap_err("header task")?;
                done.insert(start, (result, started));
            }
            for (start, _) in &plan {
                if *start != next || *stop.borrow() {
                    break;
                }
                let (result, started) = done
                    .remove(start)
                    .ok_or_else(|| eyre::eyre!("planned headers {start} did not finish"))?;
                match result {
                    Ok(records) => {
                        let blocks = records.len() as u64;
                        let from = records.first().map(|record| record.number);
                        let to = records.last().map(|record| record.number);
                        match sink.push(RecordBatch::Headers(Arc::from(records))).await? {
                            SinkReply::Applied => {
                                let (Some(from), Some(to)) = (from, to) else {
                                    return Err(eyre::eyre!("empty header batch"));
                                };
                                let secs = started.elapsed().as_secs_f32().max(0.001);
                                info!(
                                    target: "headers",
                                    from,
                                    to,
                                    blocks,
                                    next = to + 1,
                                    rate = (blocks as f32 / secs) as u64,
                                    secs,
                                    "headers stored"
                                );
                                next = to + 1;
                                if *stop.borrow() {
                                    return Ok(());
                                }
                            }
                            SinkReply::Rejected { reason } => {
                                warn!(target: "headers", next, err = %reason, "header batch rejected");
                                if wait_or_stop(&mut stop, 2).await {
                                    return Ok(());
                                }
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        warn!(target: "headers", next, %err, "header batch failed");
                        let pause = if is_wait(&err) { 2 } else { 5 };
                        if wait_or_stop(&mut stop, pause).await {
                            return Ok(());
                        }
                        break;
                    }
                }
            }
        }
    }
}

fn is_wait(err: &eyre::Report) -> bool {
    let message = format!("{err:#}");
    message.contains("no peers") || message.contains("busy") || message.contains("peer process")
}

/// `true` when the watch is set. The sender must stay alive: a dropped sender
/// completes `changed` immediately and would skip the sleep.
async fn wait_or_stop(stop: &mut watch::Receiver<bool>, secs: u64) -> bool {
    if *stop.borrow() {
        return true;
    }
    tokio::select! {
        biased;
        changed = stop.changed() => changed.is_ok() && *stop.borrow(),
        _ = sleep(Duration::from_secs(secs)) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::HeaderSource;
    use crate::{RecordBatch, Sink, SinkReply, Source, StoreSink};
    use alloy_primitives::{b256, B256};
    use ipc::{HistoryPeer, SessionInfo};
    use model::{BlockRecord, HeaderRecord, MAINNET_GENESIS_HASH};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use store::{EntityStore, LocalStore};
    use tokio::sync::watch;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-header-source-{}-{}",
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

    #[derive(Clone)]
    struct FakePeer {
        head: u64,
        linked: bool,
        calls: Arc<Mutex<Vec<u64>>>,
        stop: watch::Sender<bool>,
    }

    impl FakePeer {
        fn record(&self, start: u64) {
            let mut calls = self.calls.lock().unwrap();
            calls.push(start);
            if calls.len() > 8 {
                let _ = self.stop.send(true);
            }
        }
    }

    impl HistoryPeer for FakePeer {
        async fn status(&self) -> eyre::Result<Vec<SessionInfo>> {
            Ok(vec![SessionInfo {
                label: "fake".to_string(),
                eth: 68,
                head: Some(self.head),
                earliest: Some(0),
            }])
        }

        async fn headers(&self, start: u64, count: u64) -> eyre::Result<Vec<HeaderRecord>> {
            self.record(start);
            if self.linked {
                return Ok((0..count).map(|offset| linked(start + offset)).collect());
            }
            Ok(vec![HeaderRecord {
                number: start,
                hash: b256!("0909090909090909090909090909090909090909090909090909090909090909"),
                parent_hash: B256::ZERO,
                timestamp: start,
                receipts_root: B256::ZERO,
                logs_bloom: [0; 256],
            }])
        }

        async fn chunk(&self, _start: u64, _count: u64) -> eyre::Result<Vec<BlockRecord>> {
            Err(eyre::eyre!("chunk is not used"))
        }
    }

    fn linked(number: u64) -> HeaderRecord {
        HeaderRecord {
            number,
            hash: hash_at(number),
            parent_hash: if number == 0 {
                B256::ZERO
            } else {
                hash_at(number - 1)
            },
            timestamp: number,
            receipts_root: B256::ZERO,
            logs_bloom: [0; 256],
        }
    }

    fn hash_at(number: u64) -> B256 {
        if number == 0 {
            return MAINNET_GENESIS_HASH;
        }
        let mut bytes = [0_u8; 32];
        bytes[24..32].copy_from_slice(&number.to_be_bytes());
        B256::from(bytes)
    }

    struct ScriptSink {
        inner: Option<StoreSink<LocalStore>>,
        stop: watch::Sender<bool>,
        starts: Arc<Mutex<Vec<u64>>>,
        reject: bool,
    }

    impl Sink for ScriptSink {
        fn name(&self) -> &str {
            "script"
        }

        fn push<'a>(
            &'a mut self,
            batch: RecordBatch,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = eyre::Result<SinkReply>> + Send + 'a>>
        {
            Box::pin(async move {
                if let RecordBatch::Headers(records) = &batch {
                    if let Some(first) = records.first() {
                        self.starts.lock().unwrap().push(first.number);
                    }
                }
                let reply = match &mut self.inner {
                    Some(inner) => inner.push(batch).await?,
                    None => SinkReply::Applied,
                };
                if self.reject {
                    assert!(matches!(reply, SinkReply::Rejected { .. }));
                }
                if self.starts.lock().unwrap().len() >= 2 || !self.reject {
                    let _ = self.stop.send(true);
                }
                Ok(reply)
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rejected_does_not_advance_the_source_next() {
        let dir = TempDir::new();
        let path = dir.0.join("headers.redb");
        let (stop_tx, stop_rx) = watch::channel(false);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let starts = Arc::new(Mutex::new(Vec::new()));
        let peer = FakePeer {
            head: 32 + 512,
            linked: false,
            calls: Arc::clone(&calls),
            stop: stop_tx.clone(),
        };
        let store = LocalStore::open_headers(&path).unwrap();
        let source = HeaderSource::new(peer, store.clone());
        let mut sink = ScriptSink {
            inner: Some(StoreSink::new(store)),
            stop: stop_tx,
            starts: Arc::clone(&starts),
            reject: true,
        };
        source.run(&mut sink, stop_rx).await.unwrap();
        assert_eq!(starts.lock().unwrap().as_slice(), &[0, 0]);
        let fetched = calls.lock().unwrap().clone();
        assert_eq!(fetched.iter().filter(|start| **start == 0).count(), 2);
        assert!(fetched.iter().all(|start| *start == 0 || *start == 512));
        drop(sink);
        let store = LocalStore::open_headers(&path).unwrap();
        assert_eq!(store.header_cursor().await.unwrap(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_temp_file_append_links_parents() {
        let dir = TempDir::new();
        let path = dir.0.join("headers.redb");
        let (stop_tx, stop_rx) = watch::channel(false);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let peer = FakePeer {
            head: 33,
            linked: true,
            calls: Arc::clone(&calls),
            stop: stop_tx.clone(),
        };
        let store = LocalStore::open_headers(&path).unwrap();
        let source = HeaderSource::new(peer, store.clone());
        let mut sink = ScriptSink {
            inner: Some(StoreSink::new(store)),
            stop: stop_tx,
            starts: Arc::new(Mutex::new(Vec::new())),
            reject: false,
        };
        source.run(&mut sink, stop_rx).await.unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), &[0]);
        drop(sink);
        let store = LocalStore::open_headers(&path).unwrap();
        assert_eq!(store.header_cursor().await.unwrap(), 2);
        assert_eq!(store.header_parent(1).await.unwrap(), MAINNET_GENESIS_HASH);
        assert_eq!(store.header_parent(2).await.unwrap(), hash_at(1));
        let mut rows = Vec::new();
        store
            .for_each_header(0, 2, &mut |batch| {
                rows.extend(batch.iter().cloned());
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(rows[0].hash, MAINNET_GENESIS_HASH);
        assert_eq!(rows[1].parent_hash, rows[0].hash);
        assert_eq!(rows[1].number, 1);
        assert!(!dir.0.join("corpus.redb").exists());
        assert!(!dir.0.join("venues.redb").exists());
    }

    #[tokio::test(start_paused = true)]
    async fn stop_does_not_push_a_later_batch_or_fetch_again() {
        let dir = TempDir::new();
        let path = dir.0.join("headers.redb");
        let (stop_tx, stop_rx) = watch::channel(false);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let starts = Arc::new(Mutex::new(Vec::new()));
        let peer = FakePeer {
            head: 32 + 512,
            linked: true,
            calls: Arc::clone(&calls),
            stop: stop_tx.clone(),
        };
        let store = LocalStore::open_headers(&path).unwrap();
        let source = HeaderSource::new(peer, store);
        let mut sink = ScriptSink {
            inner: None,
            stop: stop_tx,
            starts: Arc::clone(&starts),
            reject: false,
        };
        source.run(&mut sink, stop_rx).await.unwrap();
        assert_eq!(starts.lock().unwrap().as_slice(), &[0]);
        let mut fetched = calls.lock().unwrap().clone();
        fetched.sort_unstable();
        assert_eq!(fetched, vec![0, 512]);
        drop(sink);
        let store = LocalStore::open_headers(&path).unwrap();
        assert_eq!(store.header_cursor().await.unwrap(), 0);
    }
}
