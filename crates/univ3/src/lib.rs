//! Uniswap v3 pool identities.
//!
//! A row is the pool contract plus the fee and tick spacing from `PoolCreated`.
//! Tick liquidity is left for a later RPC read against the pool. The file write
//! is [`store::service::Pools`].

use alloy_primitives::{address, Address};
use discover::univ3_pool;
use model::{decode_block_record, logs_in_block};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use store::service::{Corpus, Pools};

pub use store_model::PoolRecord;

/// UniswapV3Factory on Ethereum mainnet.
pub const CANONICAL_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");

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
                observability::info!(blocks, pools = found.len(), "scanning");
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
        let rows: Vec<_> = self
            .pools
            .iter()
            .map(|(pool, row)| (*pool, row.clone()))
            .collect();
        Pools::replace(path, &rows)
    }

    pub fn default_path() -> PathBuf {
        Pools::default_path()
    }
}
