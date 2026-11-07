//! Transport-neutral records shared by discovery and later state sync.

use alloy_primitives::{Address, Bytes, B256};
use serde::Serialize;

/// Mainnet genesis. Block 0 of a header sync has to hash to this.
pub const MAINNET_GENESIS_HASH: B256 = alloy_primitives::b256!(
    "d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"
);

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

/// One block of receipts, as stored in the corpus.
///
/// `receipts_zstd` is a zstd frame. The decompressed bytes are a little-endian
/// count of receipts, then for each receipt a little-endian length and the
/// EIP-2718 encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRecord {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub receipts_root: B256,
    pub transactions_root: B256,
    pub receipts_zstd: Vec<u8>,
}

const BLOCK_RECORD_VERSION: u8 = 1;
const BLOCK_RECORD_HEADER: usize = 1 + 8 + 32 + 32 + 8 + 32 + 32 + 4;

/// Encode [`BlockRecord`] for the peer socket and for redb.
pub fn encode_block_record(record: &BlockRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(BLOCK_RECORD_HEADER + record.receipts_zstd.len());
    out.push(BLOCK_RECORD_VERSION);
    out.extend_from_slice(&record.number.to_le_bytes());
    out.extend_from_slice(record.hash.as_slice());
    out.extend_from_slice(record.parent_hash.as_slice());
    out.extend_from_slice(&record.timestamp.to_le_bytes());
    out.extend_from_slice(record.receipts_root.as_slice());
    out.extend_from_slice(record.transactions_root.as_slice());
    let len = u32::try_from(record.receipts_zstd.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&record.receipts_zstd[..len as usize]);
    out
}

/// Decode one record. The rest of `bytes` is left for the next record.
pub fn decode_block_record(bytes: &[u8]) -> Result<(BlockRecord, usize), &'static str> {
    if bytes.len() < BLOCK_RECORD_HEADER {
        return Err("block record truncated");
    }
    if bytes[0] != BLOCK_RECORD_VERSION {
        return Err("unknown block record version");
    }
    let number = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
    let hash = B256::from_slice(&bytes[9..41]);
    let parent_hash = B256::from_slice(&bytes[41..73]);
    let timestamp = u64::from_le_bytes(bytes[73..81].try_into().unwrap());
    let receipts_root = B256::from_slice(&bytes[81..113]);
    let transactions_root = B256::from_slice(&bytes[113..145]);
    let body_len = u32::from_le_bytes(bytes[145..149].try_into().unwrap()) as usize;
    let end = BLOCK_RECORD_HEADER
        .checked_add(body_len)
        .ok_or("block record length overflow")?;
    if bytes.len() < end {
        return Err("block record body truncated");
    }
    Ok((
        BlockRecord {
            number,
            hash,
            parent_hash,
            timestamp,
            receipts_root,
            transactions_root,
            receipts_zstd: bytes[BLOCK_RECORD_HEADER..end].to_vec(),
        },
        end,
    ))
}

/// Header fields needed to walk the chain and test a logs bloom.
///
/// The value is fixed width. The block number is also the redb key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderRecord {
    pub number: u64,
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub receipts_root: B256,
    pub logs_bloom: [u8; 256],
}

const HEADER_RECORD_VERSION: u8 = 1;
const HEADER_RECORD_LEN: usize = 1 + 8 + 32 + 32 + 8 + 32 + 256;

/// Encode [`HeaderRecord`]. The layout is version, number, hash, parent,
/// timestamp, receipts root, logs bloom.
pub fn encode_header_record(record: &HeaderRecord) -> [u8; HEADER_RECORD_LEN] {
    let mut out = [0u8; HEADER_RECORD_LEN];
    out[0] = HEADER_RECORD_VERSION;
    out[1..9].copy_from_slice(&record.number.to_le_bytes());
    out[9..41].copy_from_slice(record.hash.as_slice());
    out[41..73].copy_from_slice(record.parent_hash.as_slice());
    out[73..81].copy_from_slice(&record.timestamp.to_le_bytes());
    out[81..113].copy_from_slice(record.receipts_root.as_slice());
    out[113..369].copy_from_slice(&record.logs_bloom);
    out
}

pub fn decode_header_record(bytes: &[u8]) -> Result<HeaderRecord, &'static str> {
    if bytes.len() != HEADER_RECORD_LEN {
        return Err("header record length");
    }
    if bytes[0] != HEADER_RECORD_VERSION {
        return Err("unknown header record version");
    }
    let mut logs_bloom = [0u8; 256];
    logs_bloom.copy_from_slice(&bytes[113..369]);
    Ok(HeaderRecord {
        number: u64::from_le_bytes(bytes[1..9].try_into().unwrap()),
        hash: B256::from_slice(&bytes[9..41]),
        parent_hash: B256::from_slice(&bytes[41..73]),
        timestamp: u64::from_le_bytes(bytes[73..81].try_into().unwrap()),
        receipts_root: B256::from_slice(&bytes[81..113]),
        logs_bloom,
    })
}

/// Concatenate [`encode_header_record`] values.
pub fn encode_header_records(records: &[HeaderRecord]) -> Vec<u8> {
    let mut out = Vec::with_capacity(records.len() * HEADER_RECORD_LEN);
    for record in records {
        out.extend_from_slice(&encode_header_record(record));
    }
    out
}

pub fn decode_header_records(bytes: &[u8]) -> Result<Vec<HeaderRecord>, &'static str> {
    if !bytes.len().is_multiple_of(HEADER_RECORD_LEN) {
        return Err("header records truncated");
    }
    let mut records = Vec::with_capacity(bytes.len() / HEADER_RECORD_LEN);
    for chunk in bytes.chunks(HEADER_RECORD_LEN) {
        records.push(decode_header_record(chunk)?);
    }
    Ok(records)
}

/// Decode a concatenation of [`encode_block_record`] values.
pub fn decode_block_records(mut bytes: &[u8]) -> Result<Vec<BlockRecord>, &'static str> {
    let mut records = Vec::new();
    while !bytes.is_empty() {
        let (record, used) = decode_block_record(bytes)?;
        records.push(record);
        bytes = &bytes[used..];
    }
    Ok(records)
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
