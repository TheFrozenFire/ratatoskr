//! Stored receipt blocks.

use redb::ReadableTable;
use store_model::{decode_block_record, BlockRecord};

use super::{Corpus, BLOCKS};

impl Corpus {
    /// One stored block. A missing row and an empty hole are both `None`.
    pub fn read_block(&self, number: u64) -> eyre::Result<Option<BlockRecord>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(BLOCKS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let Some(value) = table.get(number)? else {
            return Ok(None);
        };
        let bytes = value.value();
        if bytes.is_empty() {
            return Ok(None);
        }
        let (record, consumed) =
            decode_block_record(bytes).map_err(|err| eyre::eyre!("block {number}: {err}"))?;
        if consumed != bytes.len() {
            return Err(eyre::eyre!("block {number} has trailing bytes"));
        }
        if record.receipts_zstd.is_empty() {
            return Ok(None);
        }
        Ok(Some(record))
    }

    /// Copy block rows from `from` upward until `byte_budget` bytes are held.
    ///
    /// The read transaction ends before this returns. One row is always
    /// included when any remain, so a single large block still makes progress.
    pub fn read_block_page(
        &self,
        from: u64,
        byte_budget: usize,
    ) -> eyre::Result<Vec<(u64, Vec<u8>)>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(BLOCKS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut out = Vec::new();
        let mut held = 0usize;
        for item in table.range(from..)? {
            let (key, value) = item?;
            let bytes = value.value().to_vec();
            held = held.saturating_add(bytes.len());
            out.push((key.value(), bytes));
            if held >= byte_budget {
                break;
            }
        }
        Ok(out)
    }

    /// Visit every stored block in number order, including holes.
    pub fn for_each_block(
        &self,
        mut visit: impl FnMut(u64, &[u8]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(BLOCKS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        for row in table.iter()? {
            let (key, value) = row?;
            visit(key.value(), value.value())?;
        }
        Ok(())
    }
}
