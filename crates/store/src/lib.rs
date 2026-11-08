//! Owner of the three entity files.
//!
//! Header append and `write_records` stay Eventual. Venue commits, hit commits,
//! era bodies, `put_blocks`, `put_meta`, and `write_hole` stay Immediate.
//! Those choices live in `HeaderChain`, `Corpus`, and `Catalog`.

use alloy_primitives::B256;
use chain::HeaderChain;
use corpus::Corpus;
use eyre::WrapErr;
use model::{BlockRecord, HeaderRecord};
use std::path::Path;
use std::sync::Arc;
use venues::{Catalog, Put, ReceiptBody, VenueRow};

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
}

fn short_header_range(seen: u64, from: u64, to: u64) -> eyre::Report {
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
    use super::{EntityStore, LocalStore};
    use alloy_primitives::{address, b256, Address, B256};
    use model::{BlockRecord, HeaderRecord, MAINNET_GENESIS_HASH};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use venues::{Put, VenueRow};

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
        let err = store.put_blocks(&[]).await.unwrap_err();
        assert!(err.to_string().contains("corpus is not open"));
        let err = store.cursor("list").await.unwrap_err();
        assert!(err.to_string().contains("venues are not open"));
        assert_eq!(store.header_cursor().await.unwrap(), 0);
    }
}
