//! Receipt writes and holes.

use redb::{Durability, ReadableTable};
use store_model::{block_record_timestamp, encode_block_record, BlockRecord};

use super::{Corpus, BLOCKS, META};

const PRUNE_BATCH: usize = 1024;

struct Dropped {
    number: u64,
    len: u64,
    hole: bool,
}

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

    /// Delete holes and blocks whose timestamp is outside `archival`.
    ///
    /// `now` is a unix timestamp. A value that is not a block record stays.
    /// Each batch commits on its own, and `meta.bytes` shrinks with that batch.
    pub fn prune_blocks(&self, archival: &config::Archival, now: u64) -> eyre::Result<u64> {
        self.prune_in_batches(archival, now, PRUNE_BATCH)
    }

    pub(super) fn prune_in_batches(
        &self,
        archival: &config::Archival,
        now: u64,
        batch_limit: usize,
    ) -> eyre::Result<u64> {
        let mut removed = 0_u64;
        loop {
            let batch = self.prune_candidates(archival, now, batch_limit)?;
            if batch.is_empty() {
                break;
            }
            let full = batch.len() == batch_limit;
            removed += self.delete_blocks(&batch)?;
            if !full {
                break;
            }
        }
        Ok(removed)
    }

    fn prune_candidates(
        &self,
        archival: &config::Archival,
        now: u64,
        limit: usize,
    ) -> eyre::Result<Vec<Dropped>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let read = self.db.begin_read()?;
        let table = match read.open_table(BLOCKS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut batch = Vec::new();
        for item in table.range::<u64>(..)? {
            let (key, value) = item?;
            let bytes = value.value();
            let hole = bytes.is_empty();
            let drop_row = if hole {
                true
            } else {
                match block_record_timestamp(bytes) {
                    Some(timestamp) => !archival.keeps_block(timestamp, now),
                    None => false,
                }
            };
            if drop_row {
                batch.push(Dropped {
                    number: key.value(),
                    len: bytes.len() as u64,
                    hole,
                });
                if batch.len() == limit {
                    break;
                }
            }
        }
        Ok(batch)
    }

    fn delete_blocks(&self, batch: &[Dropped]) -> eyre::Result<u64> {
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        let removed = {
            let mut blocks = txn.open_table(BLOCKS)?;
            let mut meta = txn.open_table(META)?;
            let mut bytes_removed = 0_u64;
            let mut holes_removed = 0_u64;
            let mut removed = 0_u64;
            for row in batch {
                if blocks.remove(row.number)?.is_some() {
                    removed += 1;
                    bytes_removed += row.len;
                    if row.hole {
                        holes_removed += 1;
                    }
                }
            }
            let stored = meta.get("bytes")?.map(|value| value.value()).unwrap_or(0);
            meta.insert("bytes", stored.saturating_sub(bytes_removed))?;
            if holes_removed > 0 {
                let holes = meta.get("holes")?.map(|value| value.value()).unwrap_or(0);
                meta.insert("holes", holes.saturating_sub(holes_removed))?;
            }
            removed
        };
        txn.commit()?;
        Ok(removed)
    }
}
