//! Receipt corpus service.
//!
//! One redb row per block. The value is a [`store_model::BlockRecord`].
//! An empty value is a hole: peers would not serve that block. `meta.bytes`
//! is the sum of stored record lengths, and `meta.cursor-<range>` is the next
//! block a named ingest range still needs.

mod read;
mod write;

use eyre::WrapErr;
use redb::TableDefinition;
use std::path::Path;

const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

pub struct Corpus {
    db: crate::storage::File,
}

impl Corpus {
    pub fn create(path: &Path) -> eyre::Result<Self> {
        Self::create_with(path, config::Store::default().corpus)
    }

    pub fn create_with(path: &Path, cache: usize) -> eyre::Result<Self> {
        Ok(Self {
            db: crate::storage::File::create(path, cache).wrap_err("create redb")?,
        })
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn cache_reading(&self) -> observability::cache::CacheReading {
        self.db.reading()
    }

    /// Open a corpus another process may be writing. Callers only read.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        Self::open_with(path, config::Store::default().corpus)
    }

    pub fn open_with(path: &Path, cache: usize) -> eyre::Result<Self> {
        Ok(Self {
            db: crate::storage::File::read(path, cache).wrap_err("open corpus")?,
        })
    }

    pub fn meta(&self, key: &str) -> eyre::Result<Option<u64>> {
        meta_get(&self.db, key)
    }

    pub fn put_meta(&self, key: &str, value: u64) -> eyre::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            meta.insert(key, value)?;
        }
        txn.commit()?;
        Ok(())
    }

    pub fn default_path() -> std::path::PathBuf {
        crate::storage::data_file("corpus.redb")
    }

    /// Rewrite the file so pruned blocks are not left allocated.
    pub fn compact(&mut self) -> eyre::Result<bool> {
        self.db.compact()
    }
}

fn meta_get(db: &crate::storage::File, key: &str) -> eyre::Result<Option<u64>> {
    let txn = db.begin_read()?;
    let table = match txn.open_table(META) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(table.get(key)?.map(|value| value.value()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;
    use store_model::BlockRecord;

    fn sample(number: u64) -> BlockRecord {
        BlockRecord {
            number,
            hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            parent_hash: b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            timestamp: 1,
            receipts_root: alloy_primitives::B256::ZERO,
            transactions_root: alloy_primitives::B256::ZERO,
            receipts_zstd: vec![1, 2, 3],
        }
    }

    #[test]
    fn records_advance_the_cursor_and_a_hole_is_empty() {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-corpus-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let corpus = Corpus::create(&path).unwrap();
        let added = corpus.write_records("cursor-test", &[sample(7)]).unwrap();
        assert!(added > 0);
        assert_eq!(corpus.meta("cursor-test").unwrap(), Some(8));
        assert_eq!(corpus.meta("bytes").unwrap(), Some(added));
        corpus.write_hole(8).unwrap();
        assert_eq!(corpus.meta("holes").unwrap(), Some(1));
        let mut seen = Vec::new();
        corpus
            .for_each_block(|number, bytes| {
                seen.push((number, bytes.len()));
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, vec![(7, added as usize), (8, 0)]);
        assert!(corpus.read_block(8).unwrap().is_none());
        assert_eq!(corpus.read_block(7).unwrap().unwrap().number, 7);
        let cursor = corpus.meta("cursor-test").unwrap();
        corpus.put_blocks(&[sample(9)]).unwrap();
        assert_eq!(corpus.meta("cursor-test").unwrap(), cursor);
        assert_eq!(corpus.read_block(9).unwrap().unwrap().number, 9);
        corpus.write_hole(10).unwrap();
        corpus.put_blocks(&[sample(10)]).unwrap();
        assert_eq!(corpus.read_block(10).unwrap().unwrap().number, 10);
        drop(corpus);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_block_page_stops_at_the_byte_budget_and_resumes() {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-corpus-page-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let corpus = Corpus::create(&path).unwrap();
        corpus
            .write_records("cursor-page", &[sample(1), sample(2), sample(3)])
            .unwrap();
        let first = corpus.read_block_page(0, 1).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0, 1);
        let next = corpus.read_block_page(first[0].0 + 1, 1).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].0, 2);
        let rest = corpus.read_block_page(next[0].0 + 1, 1).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].0, 3);
        assert!(corpus.read_block_page(rest[0].0 + 1, 1).unwrap().is_empty());
        drop(corpus);
        let _ = std::fs::remove_file(&path);
    }

    fn timed(number: u64, timestamp: u64) -> BlockRecord {
        let mut record = sample(number);
        record.timestamp = timestamp;
        record
    }

    #[test]
    fn prune_keeps_a_block_inside_the_window_and_an_undecodable_value() {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-corpus-prune-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let mut corpus = Corpus::create(&path).unwrap();
        let now = 1_700_000_000_u64;
        let window = 100_u64;
        corpus
            .put_blocks(&[timed(1, now - window - 1), timed(2, now - window)])
            .unwrap();
        corpus.write_hole(3).unwrap();
        let txn = corpus.db.begin_write().unwrap();
        {
            let mut blocks = txn.open_table(BLOCKS).unwrap();
            blocks.insert(4_u64, &[9_u8][..]).unwrap();
        }
        txn.commit().unwrap();
        let kept = corpus.meta("bytes").unwrap().unwrap();
        let removed = corpus
            .prune_in_batches(
                &config::Archival {
                    blocks_secs: window,
                },
                now,
                1,
            )
            .unwrap();
        assert_eq!(removed, 2);
        assert!(corpus.read_block(1).unwrap().is_none());
        assert_eq!(
            corpus.read_block(2).unwrap().unwrap().timestamp,
            now - window
        );
        assert_eq!(corpus.meta("holes").unwrap(), Some(0));
        assert_eq!(corpus.meta("bytes").unwrap(), Some(kept / 2));
        let dropped = corpus
            .prune_blocks(&config::Archival::default(), now)
            .unwrap();
        assert_eq!(dropped, 1);
        assert!(corpus.read_block(2).unwrap().is_none());
        assert_eq!(corpus.meta("bytes").unwrap(), Some(0));
        let compacted = corpus.compact().unwrap();
        let _ = compacted;
        let mut seen = Vec::new();
        corpus
            .for_each_block(|number, bytes| {
                seen.push((number, bytes.len()));
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, vec![(4, 1)]);
        drop(corpus);
        let _ = std::fs::remove_file(&path);
    }
}
