//! Extending the checked header chain.

use alloy_primitives::B256;
use redb::{Durability, ReadableTable};
use store_model::{decode_header_record, encode_header_record, HeaderRecord, MAINNET_GENESIS_HASH};

use super::{HeaderChain, HEADERS, META};

impl HeaderChain {
    /// First block number that is not stored. A new database starts at 0.
    pub fn cursor(&self) -> eyre::Result<u64> {
        Ok(super::meta_get(&self.db, "next")?.unwrap_or(0))
    }

    /// Parent hash the next header must name. Block 0 has no parent.
    pub fn parent_hash(&self, next: u64) -> eyre::Result<B256> {
        if next == 0 {
            return Ok(B256::ZERO);
        }
        let txn = self.db.begin_read()?;
        let table = txn.open_table(HEADERS)?;
        let row = table
            .get(next - 1)?
            .ok_or_else(|| eyre::eyre!("cursor {next} has no parent header"))?;
        let record = decode_header_record(row.value()).map_err(|err| eyre::eyre!(err))?;
        Ok(record.hash)
    }

    /// Store `records` when they continue the chain at `next`.
    ///
    /// `parent` is the hash of block `next - 1`, or zero when `next` is zero.
    /// The returned header is the last one written.
    pub fn append(
        &self,
        records: &[HeaderRecord],
        next: u64,
        mut parent: B256,
    ) -> eyre::Result<HeaderRecord> {
        if records.is_empty() {
            return Err(eyre::eyre!("empty header batch"));
        }
        for (expect, record) in (next..).zip(records.iter()) {
            if record.number != expect {
                return Err(eyre::eyre!(
                    "header {} arrived where {expect} belongs",
                    record.number
                ));
            }
            if record.number == 0 {
                if record.hash != MAINNET_GENESIS_HASH {
                    return Err(eyre::eyre!("block 0 is not mainnet genesis"));
                }
            } else if record.parent_hash != parent {
                return Err(eyre::eyre!(
                    "header {} parent does not match the stored chain",
                    record.number
                ));
            }
            parent = record.hash;
        }
        let last = records[records.len() - 1].clone();
        let mut txn = self.db.begin_write()?;
        txn.set_durability(Durability::Immediate)?;
        {
            let mut headers = txn.open_table(HEADERS)?;
            let mut meta = txn.open_table(META)?;
            let stored = meta.get("next")?.map(|value| value.value()).unwrap_or(0);
            let first = records[0].number;
            if stored != first {
                return Err(eyre::eyre!(
                    "header cursor is {stored}; batch starts at {first}"
                ));
            }
            for record in records {
                let encoded = encode_header_record(record);
                headers.insert(record.number, encoded.as_slice())?;
            }
            meta.insert("next", last.number.saturating_add(1))?;
        }
        txn.commit()?;
        Ok(last)
    }
}
