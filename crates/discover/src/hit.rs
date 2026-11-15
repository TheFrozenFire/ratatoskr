//! A discovery output. One log may produce several hits.

use alloy_primitives::{Address, B256};
use serde::Serialize;

/// Where a discovery hint was seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Observation {
    pub chain_id: u64,
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    pub tx_hash: Option<B256>,
    pub log_index: u64,
    /// Contract that emitted the log.
    pub emitter: Address,
}

/// How an asset identity was guessed from log shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    Erc20,
    Erc721,
    Erc1155,
    /// Named by a venue-creation event. The token standard is not known yet.
    Unclassified,
}

/// Venue identity. Pool managers such as Uniswap V4 key pools by id, not address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VenueId {
    Address(Address),
    PoolId(B256),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VenueKind {
    UniswapV2Pair,
    UniswapV3Pool,
    UniswapV4Pool,
}

/// A discovery output. One log may produce several hits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum DiscoveryHit {
    Asset {
        observation: Observation,
        asset: Address,
        standard: AssetKind,
    },
    Venue {
        observation: Observation,
        venue: VenueKind,
        id: VenueId,
        /// Currencies or tokens named by the creation event.
        assets: Vec<Address>,
    },
}
