//! Reading a span of the stored chain.

use store_model::{decode_header_record, HeaderRecord};

use super::{HeaderChain, HEADERS};

impl HeaderChain {
    /// Visit headers in `[from, to)` as one snapshot.
    ///
    /// `to` is the cursor: the first block not stored. The callback sees every
    /// stored header in order. A hole is an error, because a missing bloom
    /// must not be treated as a miss.
    pub fn visit<F>(&self, from: u64, to: u64, mut visit: F) -> eyre::Result<u64>
    where
        F: FnMut(&HeaderRecord) -> eyre::Result<()>,
    {
        if from > to {
            return Err(eyre::eyre!("header range {from}..{to} is backwards"));
        }
        let read = self.db.begin_read()?;
        let table = read.open_table(HEADERS)?;
        let mut seen = 0_u64;
        for item in table.range(from..to)? {
            let (number, value) = item?;
            let number = number.value();
            let record = decode_header_record(value.value()).map_err(|err| eyre::eyre!(err))?;
            if record.number != number {
                return Err(eyre::eyre!(
                    "header key {number} stores block {}",
                    record.number
                ));
            }
            visit(&record)?;
            seen += 1;
        }
        let span = to - from;
        if seen != span {
            return Err(eyre::eyre!(
                "header chain has {seen} rows for {span} blocks starting at {from}"
            ));
        }
        Ok(seen)
    }

    /// Copy headers in `[from, to)` and drop the read transaction before returning.
    ///
    /// A missing table is an empty slice. The caller checks that the keys are dense.
    pub fn read_range(&self, from: u64, to: u64) -> eyre::Result<Vec<HeaderRecord>> {
        if from > to {
            return Err(eyre::eyre!("header range {from}..{to} is backwards"));
        }
        let read = self.db.begin_read()?;
        let table = match read.open_table(HEADERS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut out = Vec::new();
        for item in table.range(from..to)? {
            let (number, value) = item?;
            let number = number.value();
            let record = decode_header_record(value.value()).map_err(|err| eyre::eyre!(err))?;
            if record.number != number {
                return Err(eyre::eyre!(
                    "header key {number} stores block {}",
                    record.number
                ));
            }
            out.push(record);
        }
        Ok(out)
    }
}
