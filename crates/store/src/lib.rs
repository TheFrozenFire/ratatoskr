//! Owner of the three entity files.
//!
//! Header append and `write_records` stay Eventual. Venue commits, hit commits,
//! era bodies, `put_blocks`, `put_meta`, and `write_hole` stay Immediate.
//! Those choices live in `HeaderChain`, `Corpus`, and `Catalog`.

use alloy_primitives::B256;
use chain::HeaderChain;
use corpus::Corpus;
use model::{BlockRecord, HeaderRecord};
use std::path::Path;
use std::sync::Arc;
use venues::{Catalog, Put, ReceiptBody, VenueRow};

mod remote;
mod serve;

pub use remote::RemoteStore;
pub use serve::serve;

const HEADER_BATCH: u64 = 8192;

#[allow(async_fn_in_trait)]
pub trait EntityStore: Send + Sync {
    async fn header_cursor(&self) -> eyre::Result<u64>;
    async fn header_parent(&self, next: u64) -> eyre::Result<B256>;
    async fn append_headers(&self, records: &[HeaderRecord]) -> eyre::Result<HeaderRecord>;
    async fn for_each_header(
        &self,
        from: u64,
        to: u64,
        visit: &mut dyn FnMut(&[HeaderRecord]) -> eyre::Result<()>,
    ) -> eyre::Result<u64>;

    async fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>>;
    async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64>;
    async fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64>;
    async fn write_hole(&self, number: u64) -> eyre::Result<()>;
    async fn meta(&self, key: &str) -> eyre::Result<Option<u64>>;
    async fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()>;
    async fn for_each_block(
        &self,
        visit: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()>;

    async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>>;
    async fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64>;
    async fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>>;
    async fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>>;
    async fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()>;
    async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>>;
    async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()>;
}

pub struct LocalStore {
    chain: Arc<HeaderChain>,
    corpus: Arc<Corpus>,
    catalog: Arc<Catalog>,
}

impl LocalStore {
    pub fn open(headers: &Path, corpus: &Path, venues: &Path) -> eyre::Result<Self> {
        Ok(Self {
            chain: Arc::new(HeaderChain::create(headers)?),
            corpus: Arc::new(Corpus::create(corpus)?),
            catalog: Arc::new(Catalog::open(venues)?),
        })
    }
}

impl EntityStore for LocalStore {
    async fn header_cursor(&self) -> eyre::Result<u64> {
        let chain = Arc::clone(&self.chain);
        blocking(move || chain.cursor()).await
    }

    async fn header_parent(&self, next: u64) -> eyre::Result<B256> {
        let chain = Arc::clone(&self.chain);
        blocking(move || chain.parent_hash(next)).await
    }

    async fn append_headers(&self, records: &[HeaderRecord]) -> eyre::Result<HeaderRecord> {
        let chain = Arc::clone(&self.chain);
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
            let chain = Arc::clone(&self.chain);
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
        let corpus = Arc::clone(&self.corpus);
        blocking(move || corpus.read_block(number)).await
    }

    async fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64> {
        let corpus = Arc::clone(&self.corpus);
        let records = records.to_vec();
        blocking(move || corpus.put_blocks(&records)).await
    }

    async fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64> {
        let corpus = Arc::clone(&self.corpus);
        let cursor_key = cursor_key.to_string();
        let records = records.to_vec();
        blocking(move || corpus.write_records(&cursor_key, &records)).await
    }

    async fn write_hole(&self, number: u64) -> eyre::Result<()> {
        let corpus = Arc::clone(&self.corpus);
        blocking(move || corpus.write_hole(number)).await
    }

    async fn meta(&self, key: &str) -> eyre::Result<Option<u64>> {
        let corpus = Arc::clone(&self.corpus);
        let key = key.to_string();
        blocking(move || corpus.meta(&key)).await
    }

    async fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()> {
        let corpus = Arc::clone(&self.corpus);
        let key = key.to_string();
        blocking(move || corpus.put_meta(&key, value)).await
    }

    async fn for_each_block(
        &self,
        visit: &mut dyn FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        let corpus = Arc::clone(&self.corpus);
        let rows = blocking(move || {
            let mut rows = Vec::new();
            corpus.for_each_block(|number, bytes| {
                rows.push((number, bytes.to_vec()));
                Ok(())
            })?;
            Ok(rows)
        })
        .await?;
        for (number, bytes) in &rows {
            visit(*number, bytes)?;
        }
        Ok(())
    }

    async fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
        let catalog = Arc::clone(&self.catalog);
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
        let catalog = Arc::clone(&self.catalog);
        let job = job.to_string();
        let rows = rows.to_vec();
        blocking(move || catalog.commit_chunk(&job, next, &rows, put)).await
    }

    async fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>> {
        let catalog = Arc::clone(&self.catalog);
        let job = job.to_string();
        blocking(move || catalog.hit_scan(&job)).await
    }

    async fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>> {
        let catalog = Arc::clone(&self.catalog);
        let job = job.to_string();
        blocking(move || catalog.hits_from(&job, from, to)).await
    }

    async fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()> {
        let catalog = Arc::clone(&self.catalog);
        let job = job.to_string();
        let hits = hits.to_vec();
        blocking(move || catalog.commit_hits(&job, scanned_to, &hits)).await
    }

    async fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
        let catalog = Arc::clone(&self.catalog);
        let numbers = numbers.to_vec();
        blocking(move || catalog.read_bodies(&numbers)).await
    }

    async fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
        let catalog = Arc::clone(&self.catalog);
        let bodies = bodies.to_vec();
        blocking(move || catalog.put_bodies(&bodies)).await
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
    use venues::{Put, VenueRow};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("ratatoskr-store-{}-{n}", std::process::id()));
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

        let chain = std::sync::Arc::clone(&store.chain);
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

    struct Serving {
        _dir: TempDir,
        socket: PathBuf,
        cancel: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
        remote: RemoteStore,
    }

    impl Serving {
        async fn start() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = TempDir::new();
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let socket = std::env::temp_dir().join(format!("rsk-{}-{n}.sock", std::process::id()));
            let _ = std::fs::remove_file(&socket);
            std::fs::write(&socket, b"stale").unwrap();
            let store = Arc::new(open(&dir.0));
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
}
