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

/// Receipt batches are multi-megabyte sequential reads and writes.
const PAGE_CACHE: usize = 64 * 1024 * 1024;

pub struct Corpus {
    db: crate::storage::File,
}

impl Corpus {
    pub fn create(path: &Path) -> eyre::Result<Self> {
        Ok(Self {
            db: crate::storage::File::create(path, PAGE_CACHE).wrap_err("create redb")?,
        })
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn cache_reading(&self) -> ipc::db::CacheReading {
        self.db.reading()
    }

    /// Open a corpus another process may be writing. Callers only read.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        Ok(Self {
            db: crate::storage::File::read(path, PAGE_CACHE).wrap_err("open corpus")?,
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
}
