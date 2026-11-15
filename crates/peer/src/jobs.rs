//! The slot table and the tasks that run a session's jobs.

use crate::dial::{note, serve_outcome, sighting_of};
use crate::history::{clamp_count, BatchKind, HistoryPeer, SessionInfo};
use crate::socket::range_response;
use discover::{classify, DiscoveryHit};
use eth_p2p::{fetch_block_logs, fetch_block_range, fetch_header_range, OutboundSession};
use observability::info;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use store_model::{BlockRecord, HeaderRecord};
use tokio::{
    sync::{mpsc, oneshot, Mutex},
    task::JoinSet,
};

const MAINNET: u64 = 1;

/// In-process history reads. The slot table is the one [`crate::serve`] dials.
///
/// `headers` and `chunk` move the vectors `fetch_header_range` and
/// `fetch_block_range` return. Encoding stays on the socket path.
#[derive(Clone)]
pub struct LocalPeer {
    pub(crate) slots: Arc<Mutex<Table>>,
    pub(crate) wanted: Arc<AtomicU64>,
}

impl LocalPeer {
    pub(crate) fn new() -> Self {
        Self {
            slots: Arc::new(Mutex::new(Table::new())),
            wanted: Arc::new(AtomicU64::new(0)),
        }
    }
}

impl HistoryPeer for LocalPeer {
    async fn status(&self) -> eyre::Result<Vec<SessionInfo>> {
        let guard = self.slots.lock().await;
        Ok(guard
            .slots
            .iter()
            .filter(|slot| !slot.jobs.is_closed())
            .map(|slot| SessionInfo {
                label: slot.label.clone(),
                eth: u64::from(slot.eth),
                head: slot_head(slot),
                earliest: slot.earliest,
            })
            .collect())
    }

    async fn headers(&self, start: u64, count: u64) -> eyre::Result<Vec<HeaderRecord>> {
        let count = clamp_count(BatchKind::Headers, Some(count));
        match range_response(&self.slots, &self.wanted, start, count, BatchKind::Headers).await {
            Ok(RangeRecords::Headers(records)) => Ok(records),
            Ok(RangeRecords::Blocks(_)) => Err(eyre::eyre!("header request returned blocks")),
            Err(err) => Err(eyre::eyre!(err)),
        }
    }

    async fn chunk(&self, start: u64, count: u64) -> eyre::Result<Vec<BlockRecord>> {
        let count = clamp_count(BatchKind::Chunk, Some(count));
        match range_response(&self.slots, &self.wanted, start, count, BatchKind::Chunk).await {
            Ok(RangeRecords::Blocks(records)) => Ok(records),
            Ok(RangeRecords::Headers(_)) => Err(eyre::eyre!("chunk request returned headers")),
            Err(err) => Err(eyre::eyre!(err)),
        }
    }
}

pub(crate) struct Slot {
    pub(crate) label: String,
    pub(crate) eth: u8,
    /// Shared with the session. Announcements raise it after publish.
    pub(crate) head: Arc<AtomicU64>,
    pub(crate) earliest: Option<u64>,
    pub(crate) jobs: mpsc::Sender<Job>,
}

pub(crate) fn slot_head(slot: &Slot) -> Option<u64> {
    match slot.head.load(Ordering::Relaxed) {
        0 => None,
        number => Some(number),
    }
}

pub(crate) struct Table {
    closed: bool,
    pub(crate) slots: Vec<Slot>,
}

impl Table {
    pub(crate) fn new() -> Self {
        Self {
            closed: false,
            slots: Vec::new(),
        }
    }

    pub(crate) fn close(&mut self) {
        self.closed = true;
        self.slots.clear();
    }

    /// `false` drops `slot` so a probe that lost the race does not stay connected.
    pub(crate) fn publish(&mut self, slot: Slot) -> bool {
        if self.closed {
            drop(slot);
            false
        } else {
            self.slots.push(slot);
            true
        }
    }
}

#[derive(Debug)]
pub(crate) enum RangeRecords {
    Headers(Vec<HeaderRecord>),
    Blocks(Vec<BlockRecord>),
}

pub(crate) enum Job {
    Block {
        block: u64,
        reply: oneshot::Sender<Result<Fetched, String>>,
    },
    Chunk {
        start: u64,
        count: u64,
        reply: oneshot::Sender<Result<RangeRecords, String>>,
    },
    Headers {
        start: u64,
        count: u64,
        reply: oneshot::Sender<Result<RangeRecords, String>>,
    },
}

pub(crate) struct Fetched {
    pub(crate) peer: String,
    pub(crate) eth: u8,
    pub(crate) head: Option<u64>,
    pub(crate) number: u64,
    pub(crate) hash: String,
    pub(crate) logs: usize,
    pub(crate) hits: Vec<DiscoveryHit>,
}

pub(crate) async fn run_peer(
    session: OutboundSession,
    mut jobs: mpsc::Receiver<Job>,
    sightings: mpsc::UnboundedSender<crate::catalog::Sighting>,
    chunks_per_peer: usize,
) {
    let mut inflight: JoinSet<bool> = JoinSet::new();
    let mut jobs_open = true;
    loop {
        if !jobs_open && inflight.is_empty() {
            return;
        }
        tokio::select! {
            biased;
            joined = inflight.join_next(), if !inflight.is_empty() => {
                match joined {
                    Some(Ok(true)) | Some(Err(_)) => return,
                    _ => {}
                }
            }
            job = jobs.recv(), if jobs_open && inflight.len() < chunks_per_peer => {
                match job {
                    Some(job) => {
                        let session = session.clone();
                        let sightings = sightings.clone();
                        inflight.spawn(async move { run_job(session, job, sightings).await });
                    }
                    None => jobs_open = false,
                }
            }
        }
    }
}

fn remember_serve(
    sightings: &mpsc::UnboundedSender<crate::catalog::Sighting>,
    session: &OutboundSession,
    served: Option<u64>,
    err: Option<&str>,
) {
    let outcome = match (served, err) {
        (Some(_), None) => crate::catalog::SERVED,
        (_, Some(err)) => serve_outcome(err),
        (None, None) => crate::catalog::EMPTY,
    };
    note(
        sightings,
        sighting_of(
            session.peer,
            outcome,
            session.eth_version as u8,
            session.earliest_block,
            session.latest_block,
            served,
        ),
    );
}

async fn run_job(
    session: OutboundSession,
    job: Job,
    sightings: mpsc::UnboundedSender<crate::catalog::Sighting>,
) -> bool {
    match job {
        Job::Block { block, reply } => {
            let result = fetch_one(&session, block).await;
            match &result {
                Ok(_) => remember_serve(&sightings, &session, Some(block), None),
                Err(err) => remember_serve(&sightings, &session, None, Some(err)),
            }
            let failed = result.is_err();
            let _ = reply.send(result);
            failed
        }
        Job::Chunk {
            start,
            count,
            reply,
        } => {
            let result = fetch_block_range(&session, start, count)
                .await
                .map_err(|err| format!("{err:#}"));
            // A peer can refuse one old block and still serve others.
            // Drop the socket only when the stream itself failed.
            let failed = result.as_ref().is_err_and(|err| session_is_dead(err));
            match &result {
                Ok(records) if !records.is_empty() => {
                    remember_serve(&sightings, &session, Some(start), None);
                }
                Ok(_) => remember_serve(&sightings, &session, None, None),
                Err(err) => {
                    info!(peer = %session.peer, start, count, err, "chunk failed");
                    remember_serve(&sightings, &session, None, Some(err));
                }
            }
            let _ = reply.send(result.map(RangeRecords::Blocks));
            failed
        }
        Job::Headers {
            start,
            count,
            reply,
        } => {
            let result = fetch_header_range(&session, start, count)
                .await
                .map_err(|err| format!("{err:#}"));
            let failed = result.as_ref().is_err_and(|err| session_is_dead(err));
            match &result {
                Ok(records) if !records.is_empty() => {
                    remember_serve(&sightings, &session, Some(start), None);
                }
                Ok(_) => remember_serve(&sightings, &session, None, None),
                Err(err) => {
                    info!(peer = %session.peer, start, count, err, "headers failed");
                    remember_serve(&sightings, &session, None, Some(err));
                }
            }
            let _ = reply.send(result.map(RangeRecords::Headers));
            failed
        }
    }
}

async fn fetch_one(session: &OutboundSession, block: u64) -> Result<Fetched, String> {
    let block = fetch_block_logs(session, block)
        .await
        .map_err(|err| format!("{err:#}"))?;
    let hits = block
        .logs
        .iter()
        .flat_map(|log| classify(MAINNET, log))
        .collect::<Vec<_>>();
    Ok(Fetched {
        peer: session.peer.to_string(),
        eth: session.eth_version as u8,
        head: session.latest_block,
        number: block.number,
        hash: block.hash.to_string(),
        logs: block.logs.len(),
        hits,
    })
}

pub(crate) fn session_is_dead(err: &str) -> bool {
    err.contains("disconnected")
        || err.contains("peer closed")
        || err.contains("eth message")
        || err.contains("stream")
        || err.contains("Connection reset")
        || err.contains("broken pipe")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;
    use store_model::{decode_header_records, encode_header_records};

    fn header_bytes(number: u64) -> Vec<u8> {
        let mut out = vec![0u8; 369];
        out[0] = 1;
        out[1..9].copy_from_slice(&number.to_le_bytes());
        out
    }

    #[tokio::test]
    async fn local_peer_returns_fetched_headers_without_a_second_codec() {
        let slots = Arc::new(Mutex::new(Table::new()));
        let wanted = Arc::new(AtomicU64::new(u64::MAX));
        let seen = Arc::new(StdMutex::new(None));
        let (tx, mut rx) = mpsc::channel(4);
        slots.lock().await.slots.push(Slot {
            label: "fake".into(),
            eth: 69,
            head: Arc::new(AtomicU64::new(80)),
            earliest: Some(0),
            jobs: tx,
        });
        let seen_job = Arc::clone(&seen);
        let worker = tokio::spawn(async move {
            let Job::Headers {
                start,
                count,
                reply,
            } = rx.recv().await.unwrap()
            else {
                panic!("expected a header job");
            };
            *seen_job.lock().unwrap() = Some((start, count));
            let records = decode_header_records(&header_bytes(11)).unwrap();
            reply.send(Ok(RangeRecords::Headers(records))).unwrap();
        });
        let peer = LocalPeer {
            slots,
            wanted: Arc::clone(&wanted),
        };
        let headers = peer.headers(11, 5_000).await.unwrap();
        worker.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), Some((11, 1024)));
        assert_eq!(headers, decode_header_records(&header_bytes(11)).unwrap());
        assert_eq!(encode_header_records(&headers), header_bytes(11));
        assert_eq!(wanted.load(Ordering::Relaxed), 11);
        let err = LocalPeer {
            slots: Arc::new(Mutex::new(Table::new())),
            wanted: Arc::new(AtomicU64::new(0)),
        }
        .chunk(1, 1)
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no peers connected"), "{err}");
    }

    #[tokio::test]
    async fn a_closed_slot_table_drops_the_probe_sender() {
        let table = Arc::new(Mutex::new(Table::new()));
        let (tx, mut rx) = mpsc::channel(1);
        table.lock().await.close();
        let installed = table.lock().await.publish(Slot {
            label: "probe".into(),
            eth: 68,
            head: Arc::new(AtomicU64::new(40)),
            earliest: None,
            jobs: tx,
        });
        assert!(!installed);
        assert!(table.lock().await.slots.is_empty());
        assert!(rx.recv().await.is_none());
    }
}
