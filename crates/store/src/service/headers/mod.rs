//! Header chain service.
//!
//! Each append must continue the stored parent hash. Block 0 has to be
//! mainnet genesis. Proof of work is not checked. The block number is the
//! redb key, and `meta.next` is the first block not yet stored.

mod append;
mod read;

use eyre::WrapErr;
use redb::TableDefinition;
use std::path::Path;

const HEADERS: TableDefinition<u64, &[u8]> = TableDefinition::new("headers");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

pub struct HeaderChain {
    db: crate::storage::File,
}

impl HeaderChain {
    pub fn create(path: &Path) -> eyre::Result<Self> {
        Self::create_with(path, config::Store::default().headers)
    }

    pub fn create_with(path: &Path, cache: usize) -> eyre::Result<Self> {
        Ok(Self {
            db: crate::storage::File::create(path, cache).wrap_err("create header db")?,
        })
    }

    /// Page cache configured for this file, and the counters redb has accumulated.
    pub fn cache_reading(&self) -> observability::cache::CacheReading {
        self.db.reading()
    }

    pub fn default_path() -> std::path::PathBuf {
        crate::storage::data_file("headers.redb")
    }

    /// Open a chain another process may be appending to. Callers only read.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        Self::open_with(path, config::Store::default().headers)
    }

    pub fn open_with(path: &Path, cache: usize) -> eyre::Result<Self> {
        Ok(Self {
            db: crate::storage::File::read(path, cache).wrap_err("open header chain")?,
        })
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
    use alloy_primitives::{b256, B256};
    use std::path::PathBuf;
    use store_model::{HeaderRecord, MAINNET_GENESIS_HASH};

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
        // Parallel tests can sample the same nanosecond and then fight over one redb lock.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-chain-{}-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            SEQ.fetch_add(1, Ordering::Relaxed)
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
