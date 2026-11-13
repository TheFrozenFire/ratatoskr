//! Uniswap v3 pool identities.
//!
//! A row is the pool contract plus the fee and tick spacing from `PoolCreated`.
//! Tick liquidity is left for a later RPC read against the pool.

use alloy_primitives::{address, Address, B256};
use corpus::{logs_in_block, Corpus};
use discover::univ3_pool;
use eyre::WrapErr;
use model::decode_block_record;
use redb::{Durability, TableDefinition};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const POOLS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("pools");
/// UniswapV3Factory on Ethereum mainnet.
pub const CANONICAL_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
const RECORD_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolRecord {
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub factory: Address,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    pub log_index: u64,
}

/// Pools found in one pass over a corpus.
#[derive(Debug)]
pub struct PoolScan {
    pub pools: BTreeMap<Address, PoolRecord>,
    pub blocks: u64,
    pub holes: u64,
    pub first_block: Option<u64>,
    pub last_block: Option<u64>,
    /// `PoolCreated` logs whose emitter was not `factory`.
    pub other_emitters: u64,
}

impl PoolScan {
    /// `factory` keeps one emitter. `None` keeps every `PoolCreated` log.
    pub fn from_corpus(corpus: &Corpus, factory: Option<Address>) -> eyre::Result<Self> {
        let mut found = BTreeMap::new();
        let mut blocks = 0_u64;
        let mut holes = 0_u64;
        let mut first_block = None;
        let mut last_block = None;
        let mut other_emitters = 0_u64;
        corpus.for_each_block(|number, bytes| {
            first_block.get_or_insert(number);
            last_block = Some(number);
            if bytes.is_empty() {
                holes += 1;
                return Ok(());
            }
            let (record, _) = decode_block_record(bytes).map_err(|err| eyre::eyre!(err))?;
            let logs = logs_in_block(&record)?;
            blocks += 1;
            for log in logs {
                let Some(pool) = univ3_pool(&log) else {
                    continue;
                };
                if factory.is_some_and(|factory| log.address != factory) {
                    other_emitters += 1;
                    continue;
                }
                found.entry(pool.pool).or_insert(PoolRecord {
                    token0: pool.token0,
                    token1: pool.token1,
                    fee: pool.fee,
                    tick_spacing: pool.tick_spacing,
                    factory: log.address,
                    block_number: log.block_number,
                    block_hash: log.block_hash,
                    tx_index: log.tx_index,
                    log_index: log.log_index,
                });
            }
            if blocks.is_multiple_of(10_000) {
                tracing::info!(blocks, pools = found.len(), "scanning");
            }
            Ok(())
        })?;
        Ok(Self {
            pools: found,
            blocks,
            holes,
            first_block,
            last_block,
            other_emitters,
        })
    }

    pub fn fee_counts(&self) -> BTreeMap<u32, u64> {
        let mut fees = BTreeMap::new();
        for row in self.pools.values() {
            *fees.entry(row.fee).or_default() += 1;
        }
        fees
    }

    /// Write the catalog, replacing `path` when it already exists.
    pub fn write(&self, path: &Path) -> eyre::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            std::fs::remove_file(path).wrap_err("replace previous catalog")?;
        }
        let mut builder = redb::Builder::new();
        builder.set_cache_size(16 * 1024 * 1024);
        builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
        let out = builder.create(path).wrap_err("create pool catalog")?;
        let mut write = out.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut pools = write.open_table(POOLS)?;
            for (pool, row) in &self.pools {
                let encoded = encode_row(row);
                pools.insert(pool.as_slice(), encoded.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    pub fn default_path() -> PathBuf {
        data_dir().join("univ3-pools.redb")
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

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr")
}
