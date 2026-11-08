//! Checked header chain.
//!
//! Each append must continue the stored parent hash. Block 0 has to be
//! mainnet genesis. Proof of work is not checked. The block number is the
//! redb key, and `meta.next` is the first block not yet stored.

use alloy_primitives::B256;
use eyre::WrapErr;
use model::{decode_header_record, encode_header_record, HeaderRecord, MAINNET_GENESIS_HASH};
use redb::{Database, Durability, ReadableTable, TableDefinition};
use std::path::{Path, PathBuf};

const HEADERS: TableDefinition<u64, &[u8]> = TableDefinition::new("headers");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

pub struct HeaderChain {
    db: Database,
}

impl HeaderChain {
    pub fn create(path: &Path) -> eyre::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = Database::create(path).wrap_err("create header db")?;
        Ok(Self { db })
    }

    /// First block number that is not stored. A new database starts at 0.
    pub fn cursor(&self) -> eyre::Result<u64> {
        Ok(meta_get(&self.db, "next")?.unwrap_or(0))
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
        txn.set_durability(Durability::Eventual);
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

    pub fn default_path() -> PathBuf {
        data_dir().join("headers.redb")
    }

    /// Open a chain another process may be appending to. Callers only read.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let db = Database::open(path).wrap_err("open header chain")?;
        Ok(Self { db })
    }

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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    fn record(number: u64, hash: B256, parent: B256) -> HeaderRecord {
        HeaderRecord {
            number,
            hash,
            parent_hash: parent,
            timestamp: number,
            receipts_root: B256::ZERO,
            logs_bloom: [0; 256],
        }
    }

    fn temp_chain() -> (HeaderChain, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-chain-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let chain = HeaderChain::create(&path).unwrap();
        (chain, path)
    }

    #[test]
    fn block_zero_must_be_mainnet_genesis() {
        let (chain, path) = temp_chain();
        let err = chain
            .append(
                &[record(0, B256::repeat_byte(1), B256::ZERO)],
                0,
                B256::ZERO,
            )
            .unwrap_err();
        assert!(err.to_string().contains("mainnet genesis"));
        let genesis = record(0, MAINNET_GENESIS_HASH, B256::ZERO);
        let stored = chain.append(&[genesis.clone()], 0, B256::ZERO).unwrap();
        assert_eq!(stored.hash, MAINNET_GENESIS_HASH);
        assert_eq!(chain.cursor().unwrap(), 1);
        assert_eq!(chain.parent_hash(1).unwrap(), MAINNET_GENESIS_HASH);
        let mut seen = Vec::new();
        chain
            .visit(0, 1, |header| {
                seen.push(header.number);
                Ok(())
            })
            .unwrap();
        assert_eq!(seen, vec![0]);
        drop(chain);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_later_header_must_name_the_stored_parent() {
        let (chain, path) = temp_chain();
        let genesis = record(0, MAINNET_GENESIS_HASH, B256::ZERO);
        chain.append(&[genesis], 0, B256::ZERO).unwrap();
        let child = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let err = chain
            .append(
                &[record(1, child, B256::repeat_byte(2))],
                1,
                MAINNET_GENESIS_HASH,
            )
            .unwrap_err();
        assert!(err.to_string().contains("parent does not match"));
        chain
            .append(
                &[record(1, child, MAINNET_GENESIS_HASH)],
                1,
                MAINNET_GENESIS_HASH,
            )
            .unwrap();
        assert_eq!(chain.cursor().unwrap(), 2);
        let err = chain
            .append(
                &[record(1, child, MAINNET_GENESIS_HASH)],
                1,
                MAINNET_GENESIS_HASH,
            )
            .unwrap_err();
        assert!(err.to_string().contains("header cursor"));
        assert_eq!(chain.cursor().unwrap(), 2);
        drop(chain);
        let _ = std::fs::remove_file(&path);
    }
}
