//! Uniswap v3 pool file.
//!
//! One row is the pool contract plus the fee and tick spacing from `PoolCreated`.

use crate::storage::Database;
use alloy_primitives::Address;
use eyre::WrapErr;
use redb::{Durability, TableDefinition};
use std::path::Path;
use store_model::PoolRecord;

/// The histogram write is one pass over the pools gathered in memory.
const PAGE_CACHE: usize = 16 * 1024 * 1024;

const POOLS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("pools");
const RECORD_VERSION: u8 = 1;

/// `univ3-pools.redb`.
pub struct Pools;

impl Pools {
    pub fn default_path() -> std::path::PathBuf {
        crate::storage::data_file("univ3-pools.redb")
    }

    /// Replace `path` with `rows`. An existing file is removed first.
    pub fn replace(path: &Path, rows: &[(Address, PoolRecord)]) -> eyre::Result<()> {
        if path.exists() {
            std::fs::remove_file(path).wrap_err("replace previous catalog")?;
        }
        let out = Database::create(path, PAGE_CACHE).wrap_err("create pool catalog")?;
        out.watch();
        let mut write = out.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut pools = write.open_table(POOLS)?;
            for (pool, row) in rows {
                let encoded = encode_row(row);
                pools.insert(pool.as_slice(), encoded.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }
}

fn encode_row(row: &PoolRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 20 * 3 + 4 + 4 + 8 + 32 + 8 + 8);
    out.push(RECORD_VERSION);
    out.extend_from_slice(row.token0.as_slice());
    out.extend_from_slice(row.token1.as_slice());
    out.extend_from_slice(&row.fee.to_le_bytes());
    out.extend_from_slice(&row.tick_spacing.to_le_bytes());
    out.extend_from_slice(row.factory.as_slice());
    out.extend_from_slice(&row.block_number.to_le_bytes());
    out.extend_from_slice(row.block_hash.as_slice());
    out.extend_from_slice(&row.tx_index.to_le_bytes());
    out.extend_from_slice(&row.log_index.to_le_bytes());
    out
}
