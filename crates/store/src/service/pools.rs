//! Uniswap v3 pool file.
//!
//! One row is the pool contract plus the fee and tick spacing from `PoolCreated`.

use alloy_primitives::Address;
use eyre::WrapErr;
use redb::{Durability, TableDefinition};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use store_model::PoolRecord;

/// The histogram write is one pass over the pools gathered in memory.
const PAGE_CACHE: usize = 16 * 1024 * 1024;

const POOLS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("pools");
const RECORD_VERSION: u8 = 1;

/// `univ3-pools.redb`.
pub struct Pools;

impl Pools {
    pub fn default_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/ratatoskr")
            .join("univ3-pools.redb")
    }

    /// Replace `path` with `rows`. An existing file is removed first.
    pub fn replace(path: &Path, rows: &[(Address, PoolRecord)]) -> eyre::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            std::fs::remove_file(path).wrap_err("replace previous catalog")?;
        }
        let mut builder = redb::Builder::new();
        builder.set_cache_size(PAGE_CACHE);
        builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
        let out = Arc::new(builder.create(path).wrap_err("create pool catalog")?);
        ipc::db::watch_db(ipc::db::file_label(path), PAGE_CACHE as u64, &out);
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
