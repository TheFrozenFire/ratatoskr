//! Transport-neutral records shared by discovery and later state sync.

use alloy_primitives::{Address, Bytes, B256};
use serde::Serialize;

pub use store_model::{
    decode_block_record, decode_block_records, decode_header_record, decode_header_records,
    encode_block_record, encode_header_record, encode_header_records, BlockRecord, HeaderRecord,
    MAINNET_GENESIS_HASH,
};

/// Ethereum mainnet chain id. Other chains use the same record types.
pub const ETHEREUM_MAINNET: u64 = 1;

/// One log, independent of which transport produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NormalizedLog {
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    /// Absent when the transport returned receipts without bodies.
    pub tx_hash: Option<B256>,
    pub log_index: u64,
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

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

/// Logs from a single block, in chain order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLogs {
    pub number: u64,
    pub hash: B256,
    pub logs: Vec<NormalizedLog>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn block_record_roundtrip() {
        let record = BlockRecord {
            number: 12_369_621,
            hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
            parent_hash: b256!("2222222222222222222222222222222222222222222222222222222222222222"),
            timestamp: 1_620_000_000,
            receipts_root: b256!(
                "3333333333333333333333333333333333333333333333333333333333333333"
            ),
            transactions_root: b256!(
                "4444444444444444444444444444444444444444444444444444444444444444"
            ),
            receipts_zstd: vec![1, 2, 3, 4],
        };
        let bytes = encode_block_record(&record);
        let decoded = decode_block_records(&bytes).expect("decode");
        assert_eq!(decoded, vec![record]);
    }

    #[test]
    fn header_record_roundtrip() {
        let record = HeaderRecord {
            number: 0,
            hash: MAINNET_GENESIS_HASH,
            parent_hash: B256::ZERO,
            timestamp: 0,
            receipts_root: b256!(
                "56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421"
            ),
            logs_bloom: [0xab; 256],
        };
        let bytes = encode_header_records(&[record.clone()]);
        let decoded = decode_header_records(&bytes).expect("decode");
        assert_eq!(decoded, vec![record]);
    }
}
