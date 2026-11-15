//! Uniswap v3 pool file.
//!
//! One row is the pool contract plus the fee and tick spacing from `PoolCreated`.

mod row;

use crate::storage::Database;
use alloy_primitives::Address;
use eyre::WrapErr;
use redb::{Durability, TableDefinition};
use std::path::Path;
use store_model::PoolRecord;

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
        let out = Database::create(path, config::Store::default().pools)
            .wrap_err("create pool catalog")?;
        out.watch();
        let mut write = out.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut pools = write.open_table(POOLS)?;
            for (pool, row) in rows {
                let encoded = row::encode_row(row);
                pools.insert(pool.as_slice(), encoded.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }
}
