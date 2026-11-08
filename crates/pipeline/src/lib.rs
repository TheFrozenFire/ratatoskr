//! A source commits a [`RecordBatch`] by awaiting [`Sink::push`].
//!
//! The local cursor moves only when the reply is [`SinkReply::Applied`].
//! [`Fanout`] awaits required sinks in order, then `try_send`s each tee.
//! A full or closed tee is dropped and is not [`SinkReply::Rejected`].

mod header;
mod receipt;
mod venue;

pub use header::HeaderSource;
pub use receipt::ReceiptSource;
pub use venue::VenueSource;

use model::{BlockRecord, HeaderRecord};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use store::{EntityStore, LocalStore, RemoteStore};
use tokio::sync::mpsc;
use venues::{Put, ReceiptBody, VenueRow};

/// One commit. Cloning clones the `Arc`s, not the payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordBatch {
    Headers(Arc<[HeaderRecord]>),
    /// `cursor_key` set means `write_records`. `None` means `put_blocks`.
    Blocks {
        cursor_key: Option<Arc<str>>,
        records: Arc<[BlockRecord]>,
    },
    Hits {
        job: Arc<str>,
        scanned_to: u64,
        blocks: Arc<[u64]>,
    },
    Bodies(Arc<[(u64, ReceiptBody)]>),
    Venues {
        job: Arc<str>,
        next: u64,
        put: Put,
        rows: Arc<[VenueRow]>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SinkReply {
    Applied,
    Rejected { reason: String },
}

pub trait Sink: Send {
    fn name(&self) -> &str;

    /// Boxed with `std`. An `async fn` here is not dyn-compatible.
    fn push<'a>(
        &'a mut self,
        batch: RecordBatch,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<SinkReply>> + Send + 'a>>;
}

#[allow(async_fn_in_trait)]
pub trait Source: Send {
    fn name(&self) -> &str;

    /// `sink.push` is the commit. A queued send is not.
    async fn run(
        self,
        sink: &mut dyn Sink,
        stop: tokio::sync::watch::Receiver<bool>,
    ) -> eyre::Result<()>;
}

pub struct Fanout {
    required: Vec<Box<dyn Sink>>,
    tees: Vec<mpsc::Sender<RecordBatch>>,
}

impl Fanout {
    pub fn new(required: Vec<Box<dyn Sink>>) -> Self {
        Self {
            required,
            tees: Vec::new(),
        }
    }

    /// Capacity-1 observer. A full or closed send is dropped, not rejected.
    pub fn tee(&mut self) -> mpsc::Receiver<RecordBatch> {
        let (tx, rx) = mpsc::channel(1);
        self.tees.push(tx);
        rx
    }
}

impl Sink for Fanout {
    fn name(&self) -> &str {
        "fanout"
    }

    fn push<'a>(
        &'a mut self,
        batch: RecordBatch,
    ) -> Pin<Box<dyn Future<Output = eyre::Result<SinkReply>> + Send + 'a>> {
        pin(async move {
            for sink in &mut self.required {
                match sink.push(batch.clone()).await? {
                    SinkReply::Applied => {}
                    rejected @ SinkReply::Rejected { .. } => return Ok(rejected),
                }
            }
            for tx in &self.tees {
                if let Err(err) = tx.try_send(batch.clone()) {
                    tracing::warn!(%err, "tee dropped");
                }
            }
            Ok(SinkReply::Applied)
        })
    }
}

/// Required sink. `push` calls [`EntityStore`]. It does not call `put_meta`.
pub struct StoreSink<S> {
    store: S,
}

impl<S: EntityStore> StoreSink<S> {
    pub fn new(store: S) -> Self {
        Self { store }
    }
}

// `EntityStore` does not promise `Send` futures. These stores' futures are
// `Send`, which this `dyn Sink` needs. The trait stays concrete, not `dyn`.
macro_rules! impl_store_sink {
    ($ty:ty) => {
        impl Sink for StoreSink<$ty> {
            fn name(&self) -> &str {
                "store"
            }

            fn push<'a>(
                &'a mut self,
                batch: RecordBatch,
            ) -> Pin<Box<dyn Future<Output = eyre::Result<SinkReply>> + Send + 'a>> {
                pin(async move {
                    match batch {
                        RecordBatch::Headers(records) => {
                            applied(self.store.append_headers(&records).await)
                        }
                        RecordBatch::Blocks {
                            cursor_key: Some(key),
                            records,
                        } => {
                            if key.is_empty() {
                                return Err(eyre::eyre!("empty cursor key"));
                            }
                            applied(self.store.write_records(&key, &records).await)
                        }
                        RecordBatch::Blocks {
                            cursor_key: None,
                            records,
                        } => applied(self.store.put_blocks(&records).await),
                        RecordBatch::Hits {
                            job,
                            scanned_to,
                            blocks,
                        } => applied(self.store.commit_hits(&job, scanned_to, &blocks).await),
                        RecordBatch::Bodies(bodies) => {
                            applied(self.store.put_bodies(&bodies).await)
                        }
                        RecordBatch::Venues {
                            job,
                            next,
                            put,
                            rows,
                        } => applied(self.store.commit_chunk(&job, next, &rows, put).await),
                    }
                })
            }
        }
    };
}

impl_store_sink!(LocalStore);
impl_store_sink!(RemoteStore);

fn pin<'a, T>(
    future: impl Future<Output = T> + Send + 'a,
) -> Pin<Box<dyn Future<Output = T> + Send + 'a>> {
    Box::pin(future)
}

fn applied<T>(result: eyre::Result<T>) -> eyre::Result<SinkReply> {
    match result {
        Ok(_) => Ok(SinkReply::Applied),
        Err(err) if source_retries(&err) => Ok(SinkReply::Rejected {
            reason: err.to_string(),
        }),
        Err(err) => Err(err),
    }
}

/// Parent and cursor checks are data the source can submit again.
/// `EntityStore` reports them as text, not a typed error.
fn source_retries(err: &eyre::Report) -> bool {
    let message = err.to_string();
    message.contains("parent does not match")
        || message.contains("not mainnet genesis")
        || message.contains("header cursor is")
        || message.contains("arrived where")
        || message.contains("below it")
}

#[cfg(test)]
mod tests {
    use super::{Fanout, RecordBatch, Sink, SinkReply, Source, StoreSink};
    use alloy_primitives::{address, b256, Address, B256};
    use model::{BlockRecord, HeaderRecord, MAINNET_GENESIS_HASH};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};
    use store::{EntityStore, LocalStore};
    use tokio::sync::watch;
    use venues::{Put, VenueRow};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-pipeline-{}-{}",
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

    fn open(dir: &Path) -> LocalStore {
        LocalStore::open(
            &dir.join("headers.redb"),
            &dir.join("corpus.redb"),
            &dir.join("venues.redb"),
        )
        .unwrap()
    }

    fn hits(job: &str) -> RecordBatch {
        RecordBatch::Hits {
            job: Arc::from(job),
            scanned_to: 1,
            blocks: Arc::from([1_u64]),
        }
    }

    fn stop() -> watch::Receiver<bool> {
        watch::channel(false).1
    }

    struct CursorSource {
        cursor: Arc<AtomicU64>,
        batches: Vec<RecordBatch>,
    }

    impl Source for CursorSource {
        fn name(&self) -> &str {
            "cursor"
        }

        async fn run(self, sink: &mut dyn Sink, stop: watch::Receiver<bool>) -> eyre::Result<()> {
            for batch in self.batches {
                if *stop.borrow() {
                    return Ok(());
                }
                let staged = self.cursor.load(Ordering::Relaxed);
                match sink.push(batch).await? {
                    SinkReply::Applied => {
                        self.cursor.store(staged + 1, Ordering::Relaxed);
                    }
                    SinkReply::Rejected { .. } => return Ok(()),
                }
            }
            Ok(())
        }
    }

    struct ObserveSink {
        cursor: Arc<AtomicU64>,
        during: Arc<AtomicU64>,
        reply: SinkReply,
    }

    impl Sink for ObserveSink {
        fn name(&self) -> &str {
            "observe"
        }

        fn push(
            &mut self,
            _batch: RecordBatch,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = eyre::Result<SinkReply>> + Send + '_>>
        {
            self.during
                .store(self.cursor.load(Ordering::Relaxed), Ordering::Relaxed);
            let reply = self.reply.clone();
            Box::pin(std::future::ready(Ok(reply)))
        }
    }

    #[derive(Clone, Copy)]
    enum Script {
        Applied,
        Rejected(&'static str),
        Fail(&'static str),
    }

    struct CountSink {
        hits: Arc<AtomicUsize>,
        script: Script,
    }

    impl Sink for CountSink {
        fn name(&self) -> &str {
            "count"
        }

        fn push(
            &mut self,
            _batch: RecordBatch,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = eyre::Result<SinkReply>> + Send + '_>>
        {
            self.hits.fetch_add(1, Ordering::Relaxed);
            let reply = match self.script {
                Script::Applied => Ok(SinkReply::Applied),
                Script::Rejected(reason) => Ok(SinkReply::Rejected {
                    reason: reason.to_string(),
                }),
                Script::Fail(reason) => Err(eyre::eyre!(reason)),
            };
            Box::pin(std::future::ready(reply))
        }
    }

    struct JobSink {
        jobs: Arc<Mutex<Vec<String>>>,
    }

    impl Sink for JobSink {
        fn name(&self) -> &str {
            "job"
        }

        fn push(
            &mut self,
            batch: RecordBatch,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = eyre::Result<SinkReply>> + Send + '_>>
        {
            if let RecordBatch::Hits { job, .. } = &batch {
                self.jobs.lock().unwrap().push(job.to_string());
            }
            Box::pin(std::future::ready(Ok(SinkReply::Applied)))
        }
    }

    struct ApplySink;

    impl Sink for ApplySink {
        fn name(&self) -> &str {
            "apply"
        }

        fn push(
            &mut self,
            _batch: RecordBatch,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = eyre::Result<SinkReply>> + Send + '_>>
        {
            Box::pin(std::future::ready(Ok(SinkReply::Applied)))
        }
    }

    fn header(number: u64, hash: B256, parent: B256) -> HeaderRecord {
        HeaderRecord {
            number,
            hash,
            parent_hash: parent,
            timestamp: number,
            receipts_root: B256::ZERO,
            logs_bloom: [0; 256],
        }
    }

    fn block(number: u64) -> BlockRecord {
        BlockRecord {
            number,
            hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            parent_hash: b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            timestamp: 1,
            receipts_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_zstd: vec![1, 2, 3],
        }
    }

    fn row() -> VenueRow {
        VenueRow {
            kind: 1,
            id: VenueRow::address_id(address!("1111111111111111111111111111111111111111")),
            factory: Address::ZERO,
            tokens: Vec::new(),
            fee: None,
            tick_spacing: None,
            block_number: None,
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        }
    }

    #[tokio::test]
    async fn a_source_does_not_advance_its_cursor_until_applied() {
        let cursor = Arc::new(AtomicU64::new(4));
        let during = Arc::new(AtomicU64::new(u64::MAX));
        let mut sink = ObserveSink {
            cursor: Arc::clone(&cursor),
            during: Arc::clone(&during),
            reply: SinkReply::Applied,
        };
        CursorSource {
            cursor: Arc::clone(&cursor),
            batches: vec![hits("a")],
        }
        .run(&mut sink, stop())
        .await
        .unwrap();
        assert_eq!(during.load(Ordering::Relaxed), 4);
        assert_eq!(cursor.load(Ordering::Relaxed), 5);

        let cursor = Arc::new(AtomicU64::new(4));
        let during = Arc::new(AtomicU64::new(u64::MAX));
        let mut sink = ObserveSink {
            cursor: Arc::clone(&cursor),
            during: Arc::clone(&during),
            reply: SinkReply::Rejected {
                reason: "parent does not match".to_string(),
            },
        };
        CursorSource {
            cursor: Arc::clone(&cursor),
            batches: vec![hits("a"), hits("b")],
        }
        .run(&mut sink, stop())
        .await
        .unwrap();
        assert_eq!(during.load(Ordering::Relaxed), 4);
        assert_eq!(cursor.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn a_rejected_required_sink_skips_later_required_sinks() {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let mut fanout = Fanout::new(vec![
            Box::new(CountSink {
                hits: Arc::clone(&first),
                script: Script::Rejected("parent does not match"),
            }),
            Box::new(CountSink {
                hits: Arc::clone(&second),
                script: Script::Applied,
            }),
        ]);
        let reply = fanout.push(hits("x")).await.unwrap();
        assert_eq!(
            reply,
            SinkReply::Rejected {
                reason: "parent does not match".to_string(),
            }
        );
        assert_eq!(first.load(Ordering::Relaxed), 1);
        assert_eq!(second.load(Ordering::Relaxed), 0);

        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let mut fanout = Fanout::new(vec![
            Box::new(CountSink {
                hits: Arc::clone(&first),
                script: Script::Fail("sink failed"),
            }),
            Box::new(CountSink {
                hits: Arc::clone(&second),
                script: Script::Applied,
            }),
        ]);
        let err = fanout.push(hits("x")).await.unwrap_err();
        assert!(err.to_string().contains("sink failed"));
        assert_eq!(first.load(Ordering::Relaxed), 1);
        assert_eq!(second.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn a_full_tee_does_not_fail_the_source() {
        let cursor = Arc::new(AtomicU64::new(0));
        let mut fanout = Fanout::new(vec![Box::new(ApplySink)]);
        let mut tee = fanout.tee();
        CursorSource {
            cursor: Arc::clone(&cursor),
            batches: vec![hits("one"), hits("two")],
        }
        .run(&mut fanout, stop())
        .await
        .unwrap();
        assert_eq!(cursor.load(Ordering::Relaxed), 2);
        assert!(matches!(
            tee.try_recv().unwrap(),
            RecordBatch::Hits { job, .. } if &*job == "one"
        ));
        assert!(tee.try_recv().is_err());

        drop(tee);
        CursorSource {
            cursor: Arc::clone(&cursor),
            batches: vec![hits("three")],
        }
        .run(&mut fanout, stop())
        .await
        .unwrap();
        assert_eq!(cursor.load(Ordering::Relaxed), 3);
    }

    #[tokio::test]
    async fn two_sources_do_not_share_one_fanout() {
        let left_jobs = Arc::new(Mutex::new(Vec::new()));
        let right_jobs = Arc::new(Mutex::new(Vec::new()));
        let mut left = Fanout::new(vec![Box::new(JobSink {
            jobs: Arc::clone(&left_jobs),
        })]);
        let mut right = Fanout::new(vec![Box::new(JobSink {
            jobs: Arc::clone(&right_jobs),
        })]);
        let stop = stop();
        let left_cursor = Arc::new(AtomicU64::new(0));
        let right_cursor = Arc::new(AtomicU64::new(0));
        let (left_run, right_run) = tokio::join!(
            CursorSource {
                cursor: Arc::clone(&left_cursor),
                batches: vec![hits("left")],
            }
            .run(&mut left, stop.clone()),
            CursorSource {
                cursor: Arc::clone(&right_cursor),
                batches: vec![hits("right")],
            }
            .run(&mut right, stop),
        );
        left_run.unwrap();
        right_run.unwrap();
        assert_eq!(left_jobs.lock().unwrap().as_slice(), ["left"]);
        assert_eq!(right_jobs.lock().unwrap().as_slice(), ["right"]);
        assert_eq!(left_cursor.load(Ordering::Relaxed), 1);
        assert_eq!(right_cursor.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn store_sink_put_blocks_does_not_move_a_cursor_key() {
        let dir = TempDir::new();
        {
            let mut sink = StoreSink::new(open(&dir.0));
            let headers = sink
                .push(RecordBatch::Headers(Arc::from([header(
                    0,
                    MAINNET_GENESIS_HASH,
                    B256::ZERO,
                )])))
                .await
                .unwrap();
            assert_eq!(headers, SinkReply::Applied);
            let written = sink
                .push(RecordBatch::Blocks {
                    cursor_key: Some(Arc::from("cursor-range")),
                    records: Arc::from([block(7)]),
                })
                .await
                .unwrap();
            assert_eq!(written, SinkReply::Applied);
            let cached = sink
                .push(RecordBatch::Blocks {
                    cursor_key: None,
                    records: Arc::from([block(9)]),
                })
                .await
                .unwrap();
            assert_eq!(cached, SinkReply::Applied);
        }
        let store = open(&dir.0);
        assert_eq!(store.header_cursor().await.unwrap(), 1);
        assert_eq!(store.meta("cursor-range").await.unwrap(), Some(8));
        assert_eq!(store.read_block(9).await.unwrap().unwrap().number, 9);
    }

    #[tokio::test]
    async fn a_parent_mismatch_or_a_backward_chunk_is_rejected() {
        let dir = TempDir::new();
        let mut sink = StoreSink::new(open(&dir.0));
        sink.push(RecordBatch::Headers(Arc::from([header(
            0,
            MAINNET_GENESIS_HASH,
            B256::ZERO,
        )])))
        .await
        .unwrap();
        let mismatch = sink
            .push(RecordBatch::Headers(Arc::from([header(
                1,
                b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                B256::from([9_u8; 32]),
            )])))
            .await
            .unwrap();
        assert!(matches!(
            mismatch,
            SinkReply::Rejected { reason } if reason.contains("parent does not match")
        ));
        sink.push(RecordBatch::Venues {
            job: Arc::from("list"),
            next: 10,
            put: Put::Merge,
            rows: Arc::from([row()]),
        })
        .await
        .unwrap();
        let behind = sink
            .push(RecordBatch::Venues {
                job: Arc::from("list"),
                next: 9,
                put: Put::Merge,
                rows: Arc::from([row()]),
            })
            .await
            .unwrap();
        assert!(matches!(
            behind,
            SinkReply::Rejected { reason } if reason.contains("below it")
        ));
        drop(sink);
        let store = open(&dir.0);
        assert_eq!(store.header_cursor().await.unwrap(), 1);
        assert_eq!(store.cursor("list").await.unwrap(), Some(10));
    }

    #[tokio::test]
    async fn an_empty_cursor_key_is_an_error() {
        let dir = TempDir::new();
        let mut sink = StoreSink::new(open(&dir.0));
        let err = sink
            .push(RecordBatch::Blocks {
                cursor_key: Some(Arc::from("")),
                records: Arc::from([block(7)]),
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("empty cursor key"));
        let empty: Arc<[HeaderRecord]> = Arc::from(Vec::new());
        let err = sink.push(RecordBatch::Headers(empty)).await.unwrap_err();
        assert!(err.to_string().contains("empty header batch"));
        drop(sink);
        let store = open(&dir.0);
        assert_eq!(store.meta("").await.unwrap(), None);
        assert_eq!(store.meta("cursor-range").await.unwrap(), None);
        assert_eq!(store.header_cursor().await.unwrap(), 0);
    }
}
