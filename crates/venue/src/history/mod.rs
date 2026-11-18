//! Event history for factories that keep no index.
//!
//! The local header chain already holds every logs bloom through the tip.
//! Bloom hits are written to the venue catalog as they are found. Receipts
//! for those hits come from era1 before the merge and from the peer daemon
//! after it. Each downloaded payload is stored before the cursor moves, so a
//! retry reads redb instead of the network.

mod era;
mod peer;
mod scan;

pub(crate) use self::scan::scan_blooms;
pub use self::scan::BloomScan;

use self::era::{fetch_era, EraFetch, LiveEra};
use self::peer::fetch_peer;
#[cfg(test)]
use self::peer::hit_spans;
use self::scan::PendingJob;
#[cfg(test)]
use crate::catalog::ReceiptBody;
#[cfg(test)]
use crate::logs::job_filter;
#[cfg(test)]
use alloy_primitives::Bloom;
#[cfg(test)]
use store_model::BlockRecord;

use crate::catalog::Put;
use crate::logs::{LogJob, Walk, MERGE_BLOCK};
use crate::{ChainStore, CorpusStore, DiscoveryStore};
use ::peer::HistoryPeer;
use eyre::eyre;

pub async fn ingest_all<S, P>(
    store: &S,
    peer: Option<&P>,
    jobs: &[LogJob],
    stop: &tokio::sync::watch::Receiver<bool>,
    limits: &config::Venues,
) -> eyre::Result<Vec<String>>
where
    S: ChainStore + CorpusStore + DiscoveryStore,
    P: HistoryPeer + Clone + 'static,
{
    let scan = scan_blooms(store, jobs, stop, limits).await?;
    fetch_receipts(store, peer, scan, stop, limits).await
}

pub(crate) async fn fetch_receipts<S, P>(
    store: &S,
    peer: Option<&P>,
    scan: BloomScan,
    stop: &tokio::sync::watch::Receiver<bool>,
    limits: &config::Venues,
) -> eyre::Result<Vec<String>>
where
    S: CorpusStore + DiscoveryStore,
    P: HistoryPeer + Clone + 'static,
{
    if scan.stopped {
        return Ok(Vec::new());
    }
    let end = scan.end;
    let pending = scan.jobs;
    let Some(peer) = peer else {
        return Err(eyre!("peer socket"));
    };
    let mut era = LiveEra {
        http: None,
        mirror: limits.era_mirror.clone(),
        inflight: limits.era_inflight,
    };
    let mut notes = Vec::new();
    for job in pending {
        if crate::hold_for_pause(stop).await {
            break;
        }
        if job.at >= end {
            observability::info!(job = %job.job.name, at = job.at, end, "history caught up");
            continue;
        }
        match fetch_job(store, peer, &mut era, &job, end, stop, limits).await {
            Ok(Walk::Done) => observability::info!(job = %job.job.name, "history finished"),
            Ok(Walk::Stopped { at, reason }) => {
                let note = format!("{} stopped at {at}: {reason}", job.job.name);
                observability::warn!(%note);
                notes.push(note);
            }
            Err(err) => {
                let note = format!("{}: {err:#}", job.job.name);
                observability::error!(%note);
                notes.push(note);
            }
        }
    }
    Ok(notes)
}

async fn fetch_job<S, P, E>(
    store: &S,
    peer: &P,
    era: &mut E,
    job: &PendingJob,
    end: u64,
    stop: &tokio::sync::watch::Receiver<bool>,
    limits: &config::Venues,
) -> eyre::Result<Walk>
where
    S: CorpusStore + DiscoveryStore,
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
        if let Some(stopped) = fetch_era(store, era, job, &pre, stop, limits).await? {
            return Ok(stopped);
        }
        store
            .commit_chunk(&job.job.name, MERGE_BLOCK.min(end), &[], Put::Merge)
            .await?;
    }
    if end <= MERGE_BLOCK {
        return Ok(Walk::Done);
    }
    fetch_peer(store, peer, job, &post, end, stop, limits).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{b256, Address, B256};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use store::service::Corpus;

    struct Files {
        corpus: Corpus,
        catalog: crate::Catalog,
    }

    impl CorpusStore for Files {
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
            _: u64,
            _: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
        ) -> eyre::Result<()> {
            Err(eyre::eyre!("unused"))
        }
    }

    impl DiscoveryStore for Files {
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
        async fn note(&self, _: &str) -> eyre::Result<Option<u64>> {
            Err(eyre::eyre!("discovery notes are not open"))
        }
        async fn put_note(&self, _: &str, _: u64) -> eyre::Result<()> {
            Err(eyre::eyre!("discovery notes are not open"))
        }
    }

    #[test]
    fn hit_spans_split_a_long_run_and_a_gap() {
        let hits: Vec<u64> = (10..75).collect();
        let chunk = config::Venues::default().chunk_max;
        assert_eq!(hit_spans(&hits, chunk), vec![(10, 64), (74, 1)]);
        assert_eq!(hit_spans(&[1, 2, 4], chunk), vec![(1, 4)]);
        assert_eq!(hit_spans(&[1, 63], chunk), vec![(1, 63)]);
        assert_eq!(hit_spans(&[1, 64, 65], chunk), vec![(1, 64), (65, 1)]);
        assert_eq!(hit_spans(&[1, 65], chunk), vec![(1, 1), (65, 1)]);
        assert!(hit_spans(&[], chunk).is_empty());
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
        let raw = store_model::encode_receipt_bundle(&[]);
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
        async fn status(&self) -> eyre::Result<Vec<::peer::SessionInfo>> {
            Ok(Vec::new())
        }

        async fn headers(
            &self,
            _start: u64,
            _count: u64,
        ) -> eyre::Result<Vec<store_model::HeaderRecord>> {
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
        ) -> eyre::Result<Vec<::era::EraBlock>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(missing
                .iter()
                .copied()
                .map(|number| ::era::EraBlock {
                    header: ::era::HeaderRow {
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
            catalog: {
                crate::install_catalog_hooks();
                crate::Catalog::open(&dir.0.join("venues.redb")).unwrap()
            },
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

        let limits = config::Venues::default();
        fetch_era(&store, &mut era, &era_job, &[cached_era], &stop(), &limits)
            .await
            .unwrap();
        fetch_peer(
            &store,
            &peer,
            &peer_job,
            &[cached_block],
            cached_block + 1,
            &stop(),
            &limits,
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
            &limits,
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
            &limits,
        )
        .await
        .unwrap();
        assert_eq!(era_calls.load(Ordering::Relaxed), 1);
        assert_eq!(peer_calls.load(Ordering::Relaxed), 1);
    }
}
