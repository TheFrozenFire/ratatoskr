//! Receipt corpus.
//!
//! One redb row per block. The value is a [`model::BlockRecord`].
//! An empty value is a hole: peers would not serve that block. `meta.bytes`
//! is the sum of stored record lengths, and `meta.cursor-<range>` is the next
//! block a named ingest range still needs.

mod logs;

pub use logs::{decode_receipt_bundle, encode_receipt_bundle, flatten_logs, logs_in_block};

use eyre::WrapErr;
use model::{encode_block_record, BlockRecord};
use redb::{Database, Durability, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};

const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("blocks");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

pub struct Corpus {
    db: Database,
}

impl Corpus {
    pub fn create(path: &Path) -> eyre::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path).wrap_err("create redb")?;
        Ok(Self { db })
    }

    pub fn open(path: &Path) -> eyre::Result<Self> {
        let db = Database::open(path).wrap_err("open corpus")?;
        Ok(Self { db })
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

    /// Replace the blocks in `records` and advance `cursor_key` past the last one.
    ///
    /// The returned value is the increase in stored bytes. A rewrite of a
    /// shorter record can add nothing.
    pub fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Eventual);
        let delta = {
            let mut blocks = txn.open_table(BLOCKS)?;
            let mut meta = txn.open_table(META)?;
            let mut delta: i64 = 0;
            for record in records {
                let bytes = encode_block_record(record);
                let previous = blocks.insert(record.number, bytes.as_slice())?;
                if let Some(previous) = previous {
                    delta -= previous.value().len() as i64;
                }
                delta += bytes.len() as i64;
            }
            let last = records.last().map(|record| record.number).unwrap_or(0);
            meta.insert(cursor_key, last.saturating_add(1))?;
            let stored = meta.get("bytes")?.map(|value| value.value()).unwrap_or(0);
            let stored = (stored as i64 + delta).max(0) as u64;
            meta.insert("bytes", stored)?;
            delta
        };
        txn.commit()?;
        Ok(delta.max(0) as u64)
    }

    /// Remember that `number` was not served. A later real record can replace it.
    pub fn write_hole(&self, number: u64) -> eyre::Result<()> {
        let txn = self.db.begin_write()?;
        {
            let mut meta = txn.open_table(META)?;
            let holes = meta.get("holes")?.map(|value| value.value()).unwrap_or(0);
            meta.insert("holes", holes.saturating_add(1))?;
            let mut blocks = txn.open_table(BLOCKS)?;
            if blocks.get(number)?.is_none() {
                blocks.insert(number, &[][..])?;
            }
        }
        txn.commit()?;
        Ok(())
    }

    /// Visit every stored block in number order, including holes.
    pub fn for_each_block(&self, mut visit: impl FnMut(u64, &[u8]) -> eyre::Result<()>) -> eyre::Result<()> {
        let read = self.db.begin_read()?;
        let table = read.open_table(BLOCKS)?;
        for row in table.iter()? {
            let (key, value) = row?;
            visit(key.value(), value.value())?;
        }
        Ok(())
    }

    pub fn default_path() -> PathBuf {
        data_dir().join("corpus.redb")
    }
}

fn meta_get(db: &Database, key: &str) -> eyre::Result<Option<u64>> {
    let txn = db.begin_read()?;
    let table = match txn.open_table(META) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(table.get(key)?.map(|value| value.value()))
}

fn data_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/share/ratatoskr")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

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
        drop(corpus);
        let _ = std::fs::remove_file(&path);
    }
}
