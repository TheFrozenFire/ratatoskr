//! Owner of the header chain, the receipt corpus, and the venue catalog.
//!
//! The catalog is `discovery.redb`, `entities.redb`, and `work.redb`. Header
//! append and `write_records` stay Eventual. Venue commits, hit commits, era
//! bodies, state writes, `put_blocks`, `put_meta`, and `write_hole` stay
//! Immediate. Those choices live in `HeaderChain`, `Corpus`, and `Catalog`.

use alloy_primitives::B256;
use chain::HeaderChain;
use corpus::Corpus;
use eyre::WrapErr;
use model::{BlockRecord, HeaderRecord};
use std::path::Path;
use std::sync::Arc;
use venues::{Catalog, Put, ReceiptBody, VenueRow};

mod remote;
mod serve;

pub use remote::RemoteStore;
pub use serve::{db_socket, serve};
pub use venues::EntityStore;

const HEADER_BATCH: u64 = 8192;
/// Block bytes copied out of one corpus read. The table is multiple gigabytes,
/// so the next page is read only after this one has been visited.
const BLOCK_PAGE: usize = 8 * 1024 * 1024;

/// Which corpus file to open. Ingest uses [`CorpusOpen::Create`]. A venue
/// import uses [`CorpusOpen::Open`] so a missing path still fails.
pub enum CorpusOpen<'a> {
    Open(&'a Path),
    Create(&'a Path),
    OpenOrCreate(&'a Path),
}

#[derive(Clone)]
pub struct LocalStore {
    chain: Option<Arc<HeaderChain>>,
    corpus: Option<Arc<Corpus>>,
    catalog: Option<Arc<Catalog>>,
}

impl LocalStore {
    pub fn open(headers: &Path, corpus: &Path, venues: &Path) -> eyre::Result<Self> {
        Ok(Self {
            chain: Some(Arc::new(HeaderChain::create(headers)?)),
            corpus: Some(Arc::new(Corpus::create(corpus)?)),
            catalog: Some(Arc::new(Catalog::open(venues)?)),
        })
    }

    /// Corpus file only. Ingest must not lock `headers.redb` or `venues.redb`.
    pub fn corpus(path: &Path) -> eyre::Result<Self> {
        Self::parts(None, Some(CorpusOpen::Create(path)), None)
    }

    /// Header sync creates `headers.redb` and does not lock the other entity files.
    pub fn open_headers(headers: &Path) -> eyre::Result<Self> {
        Ok(Self {
            chain: Some(Arc::new(HeaderChain::create(headers)?)),
            corpus: None,
            catalog: None,
        })
    }

    /// Open only the files this process already opens. `None` does not create
    /// that database, so a list walk does not take the header lock.
    pub fn parts(
        headers: Option<&Path>,
        corpus: Option<CorpusOpen<'_>>,
        venues: Option<&Path>,
    ) -> eyre::Result<Self> {
        let chain = match headers {
            Some(path) => Some(Arc::new(HeaderChain::open(path).wrap_err("header chain")?)),
            None => None,
        };
        let corpus = match corpus {
            Some(mode) => Some(Arc::new(open_corpus(mode)?)),
            None => None,
        };
        let catalog = match venues {
            Some(path) => Some(Arc::new(Catalog::open(path)?)),
            None => None,
        };
        Ok(Self {
            chain,
            corpus,
            catalog,
        })
    }

    fn chain_db(&self) -> eyre::Result<Arc<HeaderChain>> {
        self.chain
            .clone()
            .ok_or_else(|| eyre::eyre!("headers are not open"))
    }

    fn corpus_db(&self) -> eyre::Result<Arc<Corpus>> {
        self.corpus
            .clone()
            .ok_or_else(|| eyre::eyre!("corpus is not open"))
    }

    fn catalog_db(&self) -> eyre::Result<Arc<Catalog>> {
        self.catalog
            .clone()
            .ok_or_else(|| eyre::eyre!("venues are not open"))
    }

    pub(crate) async fn page_blocks(
        &self,
        after: Option<u64>,
        limit: usize,
    ) -> eyre::Result<Vec<(u64, Vec<u8>)>> {
        let corpus = self.corpus_db()?;
        blocking(move || corpus.page_blocks(after, limit)).await
    }

    pub(crate) async fn page_venues(
        &self,
        kind: u8,
        after: Option<B256>,
        limit: usize,
    ) -> eyre::Result<Vec<VenueRow>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.page_venues(kind, after, limit)).await
    }

    pub(crate) async fn hits_page(
        &self,
        job: &str,
        from: u64,
        to: u64,
        limit: usize,
    ) -> eyre::Result<Vec<u64>> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        blocking(move || catalog.hits_from_limit(&job, from, to, limit)).await
    }
}

fn open_corpus(mode: CorpusOpen<'_>) -> eyre::Result<Corpus> {
    match mode {
        CorpusOpen::Open(path) => Corpus::open(path),
        CorpusOpen::Create(path) => Corpus::create(path),
        CorpusOpen::OpenOrCreate(path) => {
            if path.exists() {
                Corpus::open(path).wrap_err("receipt cache")
            } else {
                Corpus::create(path).wrap_err("receipt cache")
            }
        }
    }
}

impl EntityStore for LocalStore {
    async fn header_cursor(&self) -> eyre::Result<u64> {
        let chain = self.chain_db()?;
        blocking(move || chain.cursor()).await
    }

    async fn header_parent(&self, next: u64) -> eyre::Result<B256> {
        let chain = self.chain_db()?;
        blocking(move || chain.parent_hash(next)).await
    }

    async fn append_headers(&self, records: &[HeaderRecord]) -> eyre::Result<HeaderRecord> {
        let chain = self.chain_db()?;
        let records = records.to_vec();
        blocking(move || {
            let next = chain.cursor()?;
            let parent = chain.parent_hash(next)?;
            chain.append(&records, next, parent)
        })
        .await
    }

    async fn for_each_header(
        &self,
        from: u64,
        to: u64,
        visit: &mut dyn FnMut(&[HeaderRecord]) -> eyre::Result<()>,
    ) -> eyre::Result<u64> {
        // `to` stays the caller's end. A callback that appends must not extend it.
        if from > to {
            return Err(eyre::eyre!("header range {from}..{to} is backwards"));
        }
        let mut seen = 0_u64;
        let mut at = from;
        while at < to {
            let end = at.saturating_add(HEADER_BATCH).min(to);
            let chain = self.chain_db()?;
            let batch = blocking(move || chain.read_range(at, end)).await?;
            let rows = batch.len() as u64;
            if rows != end - at {
                seen += rows;
                return Err(short_header_range(seen, from, to));
            }
            visit(&batch)?;
            seen += rows;
            at = end;
        }
        if seen != to - from {
            return Err(short_header_range(seen, from, to));
        }
        Ok(seen)
    }

    async fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>> {
        let corpus = self.corpus_db()?;
        blocking(move || corpus.read_block(number)).await
    }

    async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64> {
        let corpus = self.corpus_db()?;
        let records = records.to_vec();
        blocking(move || corpus.put_blocks(&records)).await
    }

    async fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64> {
        let corpus = self.corpus_db()?;
        let cursor_key = cursor_key.to_string();
        let records = records.to_vec();
        blocking(move || corpus.write_records(&cursor_key, &records)).await
    }

    async fn write_hole(&self, number: u64) -> eyre::Result<()> {
        let corpus = self.corpus_db()?;
        blocking(move || corpus.write_hole(number)).await
    }

    async fn meta(&self, key: &str) -> eyre::Result<Option<u64>> {
        let corpus = self.corpus_db()?;
        let key = key.to_string();
        blocking(move || corpus.meta(&key)).await
    }

    async fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()> {
        let corpus = self.corpus_db()?;
        let key = key.to_string();
        blocking(move || corpus.put_meta(&key, value)).await
    }

    async fn for_each_block(
        &self,
        visit: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        let mut from = 0_u64;
        loop {
            let corpus = self.corpus_db()?;
            let start = from;
            let page = blocking(move || corpus.read_block_page(start, BLOCK_PAGE)).await?;
            let Some((last, _)) = page.last() else {
                break;
            };
            let next = last.saturating_add(1);
            for (number, bytes) in &page {
                visit(*number, bytes)?;
            }
            if next <= from {
                break;
            }
            from = next;
        }
        Ok(())
    }

    async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        blocking(move || catalog.cursor(&job)).await
    }

    async fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        let rows = rows.to_vec();
        blocking(move || catalog.commit_chunk(&job, next, &rows, put)).await
    }

    async fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        blocking(move || catalog.hit_scan(&job)).await
    }

    async fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        blocking(move || catalog.hits_from(&job, from, to)).await
    }

    async fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let job = job.to_string();
        let hits = hits.to_vec();
        blocking(move || catalog.commit_hits(&job, scanned_to, &hits)).await
    }

    async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
        let catalog = self.catalog_db()?;
        let numbers = numbers.to_vec();
        blocking(move || catalog.read_bodies(&numbers)).await
    }

    async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let bodies = bodies.to_vec();
        blocking(move || catalog.put_bodies(&bodies)).await
    }

    async fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.read_venue(kind, id)).await
    }

    async fn reconcile(&self, now: u64) -> eyre::Result<u64> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.reconcile(now)).await
    }

    async fn claim_work(
        &self,
        stage: u8,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<venues::WorkJob>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.claim_work(stage, limit, now, lease_for)).await
    }

    async fn finish_work(
        &self,
        done: &[(u8, u8, B256)],
        failed: &[venues::WorkFail],
    ) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let done = done.to_vec();
        let failed = failed.to_vec();
        blocking(move || catalog.finish_work(&done, &failed)).await
    }

    async fn enqueue_work(&self, jobs: &[venues::WorkJob], now: u64) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let jobs = jobs.to_vec();
        blocking(move || catalog.enqueue_work(&jobs, now)).await
    }

    async fn put_immutable(
        &self,
        rows: &[venues::ImmutableRow],
    ) -> eyre::Result<Vec<venues::ImmutablePut>> {
        let catalog = self.catalog_db()?;
        let rows = rows.to_vec();
        blocking(move || catalog.put_immutable(&rows)).await
    }

    async fn put_mutable(&self, row: &venues::MutableWrite) -> eyre::Result<venues::MutableStored> {
        let catalog = self.catalog_db()?;
        let row = row.clone();
        blocking(move || catalog.put_mutable(&row)).await
    }

    async fn put_tick_pages(&self, pages: &[venues::TickPage]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let pages = pages.to_vec();
        blocking(move || catalog.put_tick_pages(&pages)).await
    }

    async fn put_curve(&self, curve: &venues::CurveRecord) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let curve = curve.clone();
        blocking(move || catalog.put_curve(&curve)).await
    }

    async fn put_quote(&self, quote: &venues::QuoteRecord) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let quote = quote.clone();
        blocking(move || catalog.put_quote(&quote)).await
    }

    async fn read_state(&self, kind: u8, id: B256) -> eyre::Result<venues::PoolState> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.read_state(kind, id)).await
    }

    async fn apply_watch(&self, through: u64, hits: &[venues::WatchHit]) -> eyre::Result<()> {
        let catalog = self.catalog_db()?;
        let hits = hits.to_vec();
        blocking(move || catalog.apply_watch(through, &hits)).await
    }

    async fn watch_through(&self) -> eyre::Result<Option<u64>> {
        let catalog = self.catalog_db()?;
        blocking(move || catalog.watch_through()).await
    }
}

pub(crate) fn short_header_range(seen: u64, from: u64, to: u64) -> eyre::Report {
    let span = to - from;
    eyre::eyre!("header chain has {seen} rows for {span} blocks starting at {from}")
}

async fn blocking<T, F>(work: F) -> eyre::Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> eyre::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| eyre::eyre!("store task: {err}"))?
}

#[cfg(test)]
mod tests {
    use super::{serve, EntityStore, LocalStore, RemoteStore};
    use alloy_primitives::{address, b256, Address, B256};
    use model::{encode_block_record, BlockRecord, HeaderRecord, MAINNET_GENESIS_HASH};
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};
    use venues::{encode_row, Put, VenueRow, UNI_V3};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            // Parallel tests can sample the same nanosecond and then fight over one redb lock.
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-store-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                SEQ.fetch_add(1, Ordering::Relaxed)
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
    async fn write_hole_does_not_move_a_cursor_and_put_meta_does() {
        let dir = TempDir::new();
        let store = open(&dir.0);
        store.put_meta("cursor-range", 4).await.unwrap();
        store.write_hole(4).await.unwrap();
        assert_eq!(store.meta("cursor-range").await.unwrap(), Some(4));
        store.put_meta("cursor-range", 3).await.unwrap();
        assert_eq!(store.meta("cursor-range").await.unwrap(), Some(3));
        let mut seen = Vec::new();
        store
            .for_each_block(&mut |number, bytes| {
                seen.push((number, bytes.to_vec()));
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(seen, vec![(4, Vec::new())]);
    }

    #[tokio::test]
    async fn put_blocks_does_not_change_a_cursor_key() {
        let dir = TempDir::new();
        let store = open(&dir.0);
        let added = store
            .write_records("cursor-range", &[block(7)])
            .await
            .unwrap();
        assert!(added > 0);
        assert_eq!(store.meta("cursor-range").await.unwrap(), Some(8));
        assert_eq!(store.meta("bytes").await.unwrap(), Some(added));
        store.put_blocks(&[block(9)]).await.unwrap();
        assert_eq!(store.meta("cursor-range").await.unwrap(), Some(8));
        assert_eq!(store.read_block(9).await.unwrap().unwrap().number, 9);
    }

    #[tokio::test]
    async fn commit_chunk_rejects_a_next_below_the_stored_cursor() {
        let dir = TempDir::new();
        let store = open(&dir.0);
        let row = row();
        store
            .commit_chunk("list", 10, std::slice::from_ref(&row), Put::Merge)
            .await
            .unwrap();
        let err = store
            .commit_chunk("list", 9, &[row], Put::Merge)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("below"));
        assert_eq!(store.cursor("list").await.unwrap(), Some(10));
    }

    #[tokio::test]
    async fn commit_hits_does_not_move_hit_scan_backward() {
        let dir = TempDir::new();
        let store = open(&dir.0);
        store.commit_hits("logs", 10, &[3, 4]).await.unwrap();
        store.commit_hits("logs", 8, &[1]).await.unwrap();
        assert_eq!(store.hit_scan("logs").await.unwrap(), Some(10));
        assert_eq!(store.hits_from("logs", 0, 10).await.unwrap(), vec![1, 3, 4]);
    }

    #[tokio::test]
    async fn for_each_header_errors_when_the_range_is_not_dense() {
        let dir = TempDir::new();
        let store = open(&dir.0);
        let child = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let last = store
            .append_headers(&[
                header(0, MAINNET_GENESIS_HASH, B256::ZERO),
                header(1, child, MAINNET_GENESIS_HASH),
            ])
            .await
            .unwrap();
        assert_eq!(last.number, 1);
        assert_eq!(store.header_cursor().await.unwrap(), 2);

        let mut called = false;
        let err = store
            .for_each_header(0, 4, &mut |_| {
                called = true;
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(!called);
        assert!(err.to_string().contains("header chain has"));
        assert!(err.to_string().contains("2 rows"));

        let mut called = false;
        let err = store
            .for_each_header(3, 1, &mut |_| {
                called = true;
                Ok(())
            })
            .await
            .unwrap_err();
        assert!(!called);
        assert!(err.to_string().contains("backwards"));

        let chain = store.chain.clone().expect("headers");
        let grandchild = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let mut numbers = Vec::new();
        let seen = store
            .for_each_header(0, 2, &mut |batch| {
                numbers.extend(batch.iter().map(|header| header.number));
                let chain = std::sync::Arc::clone(&chain);
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let result = (|| -> eyre::Result<()> {
                        let next = chain.cursor()?;
                        let parent = chain.parent_hash(next)?;
                        chain.append(&[header(next, grandchild, parent)], next, parent)?;
                        Ok(())
                    })();
                    let _ = tx.send(result);
                });
                rx.recv_timeout(std::time::Duration::from_secs(5))
                    .map_err(|_| eyre::eyre!("header write blocked; read snapshot still open"))??;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(seen, 2);
        assert_eq!(numbers, vec![0, 1]);
        assert_eq!(store.header_cursor().await.unwrap(), 3);
    }

    #[tokio::test]
    async fn for_each_block_on_a_fresh_store_is_empty() {
        let dir = TempDir::new();
        let store = open(&dir.0);
        let mut seen = 0_u64;
        store
            .for_each_block(&mut |_number, _bytes| {
                seen += 1;
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(seen, 0);
    }

    #[tokio::test]
    async fn corpus_constructor_does_not_open_headers_or_venues() {
        let dir = TempDir::new();
        let path = dir.0.join("corpus.redb");
        let store = LocalStore::corpus(&path).unwrap();
        assert!(path.exists());
        assert!(!dir.0.join("headers.redb").exists());
        assert!(!dir.0.join("venues.redb").exists());
        assert!(!dir.0.join("entities.redb").exists());
        assert!(!dir.0.join("discovery.redb").exists());
        assert!(!dir.0.join("work.redb").exists());
        let err = store.header_cursor().await.unwrap_err();
        assert!(err.to_string().contains("headers are not open"));
        let err = store.cursor("job").await.unwrap_err();
        assert!(err.to_string().contains("venues are not open"));
        assert!(store.meta("bytes").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_corpus_lock_does_not_block_opening_headers() {
        let dir = TempDir::new();
        let headers = dir.0.join("headers.redb");
        let venues = dir.0.join("venues.redb");
        let created = open(&dir.0);
        drop(created);
        let corpus = LocalStore::corpus(&dir.0.join("corpus.redb")).unwrap();
        let scan = LocalStore::parts(Some(&headers), None, Some(&venues)).unwrap();
        let err = LocalStore::parts(Some(&headers), None, None)
            .map(|_| ())
            .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("already open"), "{err}");
        drop(scan);
        LocalStore::parts(Some(&headers), None, None).unwrap();
        drop(corpus);
    }

    #[tokio::test]
    async fn for_each_block_visits_each_stored_block() {
        let dir = TempDir::new();
        let store = open(&dir.0);
        store
            .write_records("cursor-range", &[block(1), block(2), block(4)])
            .await
            .unwrap();
        let mut numbers = Vec::new();
        store
            .for_each_block(&mut |number, _bytes| {
                numbers.push(number);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(numbers, vec![1, 2, 4]);
    }

    #[tokio::test]
    async fn open_headers_does_not_open_corpus_or_venues() {
        let dir = TempDir::new();
        let headers = dir.0.join("headers.redb");
        let store = LocalStore::open_headers(&headers).unwrap();
        assert!(headers.exists());
        assert!(!dir.0.join("corpus.redb").exists());
        assert!(!dir.0.join("venues.redb").exists());
        assert!(!dir.0.join("entities.redb").exists());
        assert!(!dir.0.join("discovery.redb").exists());
        assert!(!dir.0.join("work.redb").exists());
        let err = store.put_blocks(&[]).await.unwrap_err();
        assert!(err.to_string().contains("corpus is not open"));
        let err = store.cursor("list").await.unwrap_err();
        assert!(err.to_string().contains("venues are not open"));
        assert_eq!(store.header_cursor().await.unwrap(), 0);
    }

    struct Serving {
        _dir: TempDir,
        socket: PathBuf,
        cancel: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
        remote: RemoteStore,
    }

    impl Serving {
        async fn start() -> Self {
            let dir = TempDir::new();
            let store = Arc::new(open(&dir.0));
            Self::from_store(dir, store).await
        }

        async fn from_store(dir: TempDir, store: Arc<LocalStore>) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let socket = std::env::temp_dir().join(format!("rsk-{}-{n}.sock", std::process::id()));
            let _ = std::fs::remove_file(&socket);
            std::fs::write(&socket, b"stale").unwrap();
            let path = socket.clone();
            let (cancel, cancel_rx) = tokio::sync::oneshot::channel();
            let thread = std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("runtime");
                runtime.block_on(async move {
                    tokio::select! {
                        result = serve(store, &path) => result.expect("serve"),
                        _ = cancel_rx => {}
                    }
                });
            });
            let mut ready = false;
            for _ in 0..100 {
                if tokio::net::UnixStream::connect(&socket).await.is_ok() {
                    ready = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert!(ready, "serve did not bind");
            let mut mode = 0;
            for _ in 0..50 {
                mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
                if mode == 0o600 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            assert_eq!(mode, 0o600);
            let remote = RemoteStore::new(&socket);
            Self {
                _dir: dir,
                socket,
                cancel: Some(cancel),
                thread: Some(thread),
                remote,
            }
        }
    }

    impl Drop for Serving {
        fn drop(&mut self) {
            if let Some(cancel) = self.cancel.take() {
                let _ = cancel.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    #[tokio::test]
    async fn remote_append_rejects_a_bad_parent() {
        let serving = Serving::start().await;
        let genesis = header(0, MAINNET_GENESIS_HASH, B256::ZERO);
        let stored = serving
            .remote
            .append_headers(std::slice::from_ref(&genesis))
            .await
            .unwrap();
        assert_eq!(stored, genesis);
        assert_eq!(serving.remote.header_cursor().await.unwrap(), 1);
        let bad = header(
            1,
            b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        );
        let err = serving.remote.append_headers(&[bad]).await.unwrap_err();
        assert!(err.to_string().contains("parent does not match"));
        assert_eq!(serving.remote.header_cursor().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn remote_commit_chunk_rejects_a_next_below_the_stored_cursor() {
        let serving = Serving::start().await;
        let row = row();
        serving
            .remote
            .commit_chunk("list", 10, std::slice::from_ref(&row), Put::Merge)
            .await
            .unwrap();
        let err = serving
            .remote
            .commit_chunk("list", 9, &[row], Put::Merge)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("below"));
        assert_eq!(serving.remote.cursor("list").await.unwrap(), Some(10));
    }

    #[tokio::test]
    async fn remote_put_blocks_does_not_change_a_cursor_key() {
        let serving = Serving::start().await;
        let added = serving
            .remote
            .write_records("cursor-range", &[block(7)])
            .await
            .unwrap();
        assert!(added > 0);
        assert_eq!(serving.remote.meta("cursor-range").await.unwrap(), Some(8));
        assert_eq!(serving.remote.meta("bytes").await.unwrap(), Some(added));
        serving.remote.put_blocks(&[block(9)]).await.unwrap();
        assert_eq!(serving.remote.meta("cursor-range").await.unwrap(), Some(8));
        assert_eq!(
            serving.remote.read_block(9).await.unwrap().unwrap().number,
            9
        );
    }

    #[tokio::test]
    async fn remote_put_meta_round_trip() {
        let serving = Serving::start().await;
        serving.remote.put_meta("cursor-range", 4).await.unwrap();
        assert_eq!(serving.remote.meta("cursor-range").await.unwrap(), Some(4));
        serving.remote.put_meta("cursor-range", 3).await.unwrap();
        assert_eq!(serving.remote.meta("cursor-range").await.unwrap(), Some(3));
    }

    #[tokio::test]
    async fn remote_scan_yields_a_body_and_a_hole_in_key_order() {
        let serving = Serving::start().await;
        let stored = block(7);
        serving
            .remote
            .put_blocks(std::slice::from_ref(&stored))
            .await
            .unwrap();
        serving.remote.write_hole(4).await.unwrap();
        let mut seen = Vec::new();
        serving
            .remote
            .for_each_block(&mut |number, bytes| {
                seen.push((number, bytes.to_vec()));
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(
            seen,
            vec![(4, Vec::new()), (7, encode_block_record(&stored))]
        );
    }

    #[tokio::test]
    async fn remote_scan_resumes_after_a_full_page() {
        let serving = Serving::start().await;
        let records: Vec<_> = (0..70).map(block).collect();
        serving.remote.put_blocks(&records).await.unwrap();
        let mut numbers = Vec::new();
        serving
            .remote
            .for_each_block(&mut |number, _bytes| {
                numbers.push(number);
                Ok(())
            })
            .await
            .unwrap();
        assert_eq!(numbers, (0..70).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn remote_scan_venues_returns_a_version_two_pool_row() {
        let dir = TempDir::new();
        let store =
            Arc::new(LocalStore::parts(None, None, Some(&dir.0.join("venues.redb"))).unwrap());
        let serving = Serving::from_store(dir, store).await;
        let hash = b256!("abababababababababababababababababababababababababababababababab");
        let pool = VenueRow {
            kind: UNI_V3,
            id: VenueRow::address_id(address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
            factory: address!("1F98431c8aD98523631AE4a59f267346ea31F984"),
            tokens: vec![
                address!("1111111111111111111111111111111111111111"),
                address!("2222222222222222222222222222222222222222"),
            ],
            fee: Some(3000),
            tick_spacing: Some(60),
            block_number: Some(12_369_621),
            extra: None,
            block_hash: Some(hash),
            tx_index: Some(2),
            log_index: Some(5),
        };
        let mut other = pool.clone();
        other.kind = 1;
        other.block_hash = None;
        other.tx_index = None;
        other.log_index = None;
        serving
            .remote
            .commit_chunk("history", 1, &[pool.clone(), other], Put::Merge)
            .await
            .unwrap();
        let page = serving.remote.scan_venues(UNI_V3).await.unwrap();
        assert_eq!(page, vec![pool]);
        assert_eq!(encode_row(&page[0]).unwrap()[0], 2);
        assert!(!serving._dir.0.join("univ3-pools.redb").exists());
        assert!(!serving._dir.0.join("corpus.redb").exists());
        assert!(!serving._dir.0.join("venues.redb").exists());
        assert!(serving._dir.0.join("entities.redb").exists());
        assert!(serving._dir.0.join("discovery.redb").exists());
        assert!(serving._dir.0.join("work.redb").exists());
    }
}
