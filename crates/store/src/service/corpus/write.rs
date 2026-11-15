//! Receipt writes and holes.

use redb::{Durability, ReadableTable};
use store_model::{encode_block_record, BlockRecord};

use super::{Corpus, BLOCKS, META};

impl Corpus {
    /// Replace the blocks in `records` and advance `cursor_key` past the last one.
    ///
    /// The returned value is the increase in stored bytes. A rewrite of a
    /// shorter record can add nothing.
    pub fn write_records(&self, cursor_key: &str, records: &[BlockRecord]) -> eyre::Result<u64> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
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

    /// Insert receipt blocks without moving an ingest cursor.
    ///
    /// Empty `receipts_zstd` is skipped. A hole at the same number is replaced.
    pub fn put_blocks(&self, records: &[BlockRecord]) -> eyre::Result<u64> {
        if records.is_empty() {
            return Ok(0);
        }
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        let delta = {
            let mut blocks = txn.open_table(BLOCKS)?;
            let mut meta = txn.open_table(META)?;
            let mut delta: i64 = 0;
            for record in records {
                if record.receipts_zstd.is_empty() {
                    continue;
                }
                let bytes = encode_block_record(record);
                let previous = blocks.insert(record.number, bytes.as_slice())?;
                if let Some(previous) = previous {
                    delta -= previous.value().len() as i64;
                }
                delta += bytes.len() as i64;
            }
            let stored = meta.get("bytes")?.map(|value| value.value()).unwrap_or(0);
            let stored = (stored as i64 + delta).max(0) as u64;
            meta.insert("bytes", stored)?;
            delta
        };
        txn.commit()?;
        Ok(delta.max(0) as u64)
    }
}
