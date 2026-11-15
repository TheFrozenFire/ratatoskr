//! Storage services.
//!
//! [`service`] is the interface a domain crate calls. Those services read and
//! write [`store_model`] records. [`storage`] opens the redb files and keeps
//! the handles. [`LocalStore`] opens the header chain, the receipt corpus, and
//! the venue catalog through those services. The port traits name those
//! services for a caller that does not open the files.

use alloy_primitives::Address;
use eyre::WrapErr;
use service::{Catalog, Corpus, HeaderChain};
use std::path::Path;
use std::sync::Arc;
use store_model::StateSummary;

mod entity;
pub mod service;
pub mod storage;

pub use entity::{ChainStore, ConduitStore, CorpusStore, DiscoveryStore, TokenStore, VenueStore};
pub use service::{FeedError, Feeds, SpotError, Spots};

/// Which corpus file to open. Ingest uses [`CorpusOpen::Create`]. A venue
/// import uses [`CorpusOpen::Open`] so a missing path still fails.
/// [`CorpusOpen::OpenOrCreate`] writes, including when the file already exists.
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

    /// Open only the files this process already opens. `None` leaves that file
    /// closed. A header path is opened read-only and follows a writer.
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

    pub fn state_summary(&self) -> eyre::Result<StateSummary> {
        self.catalog_db()?.state_summary()
    }

    /// Page caches for the files this store has open.
    pub fn cache_readings(&self) -> Vec<observability::cache::CacheReading> {
        let mut out = Vec::new();
        if let Some(chain) = &self.chain {
            out.push(chain.cache_reading());
        }
        if let Some(corpus) = &self.corpus {
            out.push(corpus.cache_reading());
        }
        if let Some(catalog) = &self.catalog {
            out.extend(catalog.cache_readings());
        }
        out
    }

    pub fn stored_header(&self) -> eyre::Result<u64> {
        self.chain_db()?.cursor()
    }

    pub fn corpus_gauge(&self) -> eyre::Result<(u64, u64)> {
        let corpus = self.corpus_db()?;
        Ok((
            corpus.meta("bytes")?.unwrap_or(0),
            corpus.meta("holes")?.unwrap_or(0),
        ))
    }

    /// Pair conduits already stored, with the block each merge used.
    pub fn visit_conduit_blocks(
        &self,
        visit: impl FnMut(Address, Address, u64, &[routing::SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        self.catalog_db()?.visit_conduit_blocks(visit)
    }
}

fn open_corpus(mode: CorpusOpen<'_>) -> eyre::Result<Corpus> {
    match mode {
        CorpusOpen::Open(path) => Corpus::open(path),
        CorpusOpen::Create(path) => Corpus::create(path),
        CorpusOpen::OpenOrCreate(path) => Corpus::create(path).wrap_err("receipt cache"),
    }
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
    use super::{ChainStore, CorpusStore, DiscoveryStore, LocalStore};
    use alloy_primitives::{address, b256, Address, B256};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use store_model::{BlockRecord, HeaderRecord, Put, VenueRow, MAINNET_GENESIS_HASH};

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
        let again = LocalStore::parts(Some(&headers), None, None).unwrap();
        let writer = LocalStore::open_headers(&headers).unwrap();
        let err = LocalStore::open_headers(&headers).map(|_| ()).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("already open"), "{err}");
        drop(writer);
        drop(again);
        drop(scan);
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
}
