//! Uniswap v3 pool identity stored from a `PoolCreated` log.

use alloy_primitives::{Address, B256};

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
