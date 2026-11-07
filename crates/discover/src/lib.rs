//! Classify normalized logs into asset and venue identities.
//!
//! Matching is by event signature and topic count. It does not read code or storage.

use alloy_primitives::{keccak256, Address, B256};
use model::{AssetKind, DiscoveryHit, NormalizedLog, Observation, VenueId, VenueKind};
use std::sync::OnceLock;

const TRANSFER: &str = "Transfer(address,address,uint256)";
const TRANSFER_SINGLE: &str = "TransferSingle(address,address,address,uint256,uint256)";
const TRANSFER_BATCH: &str = "TransferBatch(address,address,address,uint256[],uint256[])";
const PAIR_CREATED: &str = "PairCreated(address,address,address,uint256)";
const POOL_CREATED: &str = "PoolCreated(address,address,uint24,int24,address)";
const V4_INITIALIZE: &str =
    "Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)";

struct Topics {
    transfer: B256,
    transfer_single: B256,
    transfer_batch: B256,
    pair_created: B256,
    pool_created: B256,
    v4_initialize: B256,
}

fn topics() -> &'static Topics {
    static TOPICS: OnceLock<Topics> = OnceLock::new();
    TOPICS.get_or_init(|| Topics {
        transfer: keccak256(TRANSFER),
        transfer_single: keccak256(TRANSFER_SINGLE),
        transfer_batch: keccak256(TRANSFER_BATCH),
        pair_created: keccak256(PAIR_CREATED),
        pool_created: keccak256(POOL_CREATED),
        v4_initialize: keccak256(V4_INITIALIZE),
    })
}

/// Identity hints from one log. Empty when the log is not a watched shape.
pub fn classify(chain_id: u64, log: &NormalizedLog) -> Vec<DiscoveryHit> {
    let Some(&topic0) = log.topics.first() else {
        return Vec::new();
    };
    let known = topics();
    let observation = observation(chain_id, log);

    if topic0 == known.transfer && log.topics.len() == 3 {
        return vec![asset(observation, log.address, AssetKind::Erc20)];
    }
    if topic0 == known.transfer && log.topics.len() == 4 {
        return vec![asset(observation, log.address, AssetKind::Erc721)];
    }
    if (topic0 == known.transfer_single || topic0 == known.transfer_batch) && log.topics.len() >= 4
    {
        return vec![asset(observation, log.address, AssetKind::Erc1155)];
    }
    if topic0 == known.pair_created && log.topics.len() == 3 {
        let Some(pair) = address_word(&log.data, 0) else {
            return Vec::new();
        };
        let tokens = topic_addresses(&log.topics[1..]);
        return venue(
            observation,
            VenueKind::UniswapV2Pair,
            VenueId::Address(pair),
            tokens,
        );
    }
    if topic0 == known.pool_created && log.topics.len() == 4 {
        let Some(pool) = address_word(&log.data, 1) else {
            return Vec::new();
        };
        let tokens = topic_addresses(&log.topics[1..3]);
        return venue(
            observation,
            VenueKind::UniswapV3Pool,
            VenueId::Address(pool),
            tokens,
        );
    }
    if topic0 == known.v4_initialize && log.topics.len() == 4 {
        let tokens = topic_addresses(&log.topics[2..]);
        return venue(
            observation,
            VenueKind::UniswapV4Pool,
            VenueId::PoolId(log.topics[1]),
            tokens,
        );
    }
    Vec::new()
}

fn observation(chain_id: u64, log: &NormalizedLog) -> Observation {
    Observation {
        chain_id,
        block_number: log.block_number,
        block_hash: log.block_hash,
        tx_index: log.tx_index,
        tx_hash: log.tx_hash,
        log_index: log.log_index,
        emitter: log.address,
    }
}

fn asset(observation: Observation, asset: Address, standard: AssetKind) -> DiscoveryHit {
    DiscoveryHit::Asset {
        observation,
        asset,
        standard,
    }
}

fn venue(
    observation: Observation,
    venue: VenueKind,
    id: VenueId,
    assets: Vec<Address>,
) -> Vec<DiscoveryHit> {
    let mut hits = Vec::with_capacity(assets.len() + 1);
    hits.push(DiscoveryHit::Venue {
        observation: observation.clone(),
        venue,
        id,
        assets: assets.clone(),
    });
    for asset_address in assets {
        hits.push(asset(
            observation.clone(),
            asset_address,
            AssetKind::Unclassified,
        ));
    }
    hits
}

fn topic_addresses(topics: &[B256]) -> Vec<Address> {
    topics.iter().copied().map(address_from_word).collect()
}

fn address_from_word(word: B256) -> Address {
    Address::from_slice(&word.as_slice()[12..])
}

/// Identity of a Uniswap v3 pool from a `PoolCreated` log.
///
/// `fee` and `tick_spacing` come from the event. Tick reads later use the
/// pool address and this spacing; this function does not read pool storage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Univ3Pool {
    pub pool: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
}

/// `None` when the log is not a `PoolCreated` shape or the words do not fit.
pub fn univ3_pool(log: &NormalizedLog) -> Option<Univ3Pool> {
    let known = topics();
    if log.topics.len() != 4 || log.topics[0] != known.pool_created {
        return None;
    }
    let fee = uint24(&log.topics[3])?;
    let tick_spacing = int24_word(&log.data, 0)?;
    let pool = address_word(&log.data, 1)?;
    Some(Univ3Pool {
        pool,
        token0: address_from_word(log.topics[1]),
        token1: address_from_word(log.topics[2]),
        fee,
        tick_spacing,
    })
}

fn uint24(word: &B256) -> Option<u32> {
    let bytes = word.as_slice();
    if bytes[..29].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(u32::from_be_bytes([0, bytes[29], bytes[30], bytes[31]]))
}

/// ABI `int24`: a sign-extended 32-byte word.
fn int24_word(data: &[u8], index: usize) -> Option<i32> {
    let start = index.checked_mul(32)?;
    let word = data.get(start..start + 32)?;
    let negative = word[29] & 0x80 != 0;
    let fill = if negative { 0xff } else { 0x00 };
    if word[..29].iter().any(|byte| *byte != fill) {
        return None;
    }
    Some(i32::from_be_bytes([word[28], word[29], word[30], word[31]]))
}

fn address_word(data: &[u8], index: usize) -> Option<Address> {
    let start = index.checked_mul(32)?;
    let word = data.get(start..start + 32)?;
    Some(Address::from_slice(&word[12..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, bytes, Address, Bytes};
    use model::{AssetKind, DiscoveryHit, VenueId, VenueKind};

    fn log(topics: Vec<B256>, data: Bytes) -> NormalizedLog {
        NormalizedLog {
            block_number: 10_000_835,
            block_hash: b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            tx_index: 1,
            tx_hash: None,
            log_index: 4,
            address: address!("1111111111111111111111111111111111111111"),
            topics,
            data,
        }
    }

    fn word(last: u8) -> B256 {
        let mut out = B256::ZERO;
        out.0[31] = last;
        out
    }

    fn word_u32(value: u32) -> B256 {
        let mut out = B256::ZERO;
        out.0[28..32].copy_from_slice(&value.to_be_bytes());
        out
    }

    fn data_words(last_bytes: &[u8]) -> Bytes {
        let mut data = vec![0_u8; last_bytes.len() * 32];
        for (index, last) in last_bytes.iter().enumerate() {
            data[index * 32 + 31] = *last;
        }
        Bytes::from(data)
    }

    #[test]
    fn transfer_topic_count_splits_erc20_and_erc721() {
        let known = topics();
        let erc20 = log(vec![known.transfer, word(1), word(2)], bytes!("01"));
        let erc721 = log(
            vec![known.transfer, word(1), word(2), word(9)],
            Bytes::new(),
        );
        match &classify(1, &erc20)[..] {
            [DiscoveryHit::Asset {
                standard, asset, ..
            }] => {
                assert_eq!(*standard, AssetKind::Erc20);
                assert_eq!(*asset, erc20.address);
            }
            other => panic!("expected one erc20 hit, got {other:?}"),
        }
        match &classify(1, &erc721)[..] {
            [DiscoveryHit::Asset { standard, .. }] => assert_eq!(*standard, AssetKind::Erc721),
            other => panic!("expected one erc721 hit, got {other:?}"),
        }
    }

    #[test]
    fn erc1155_single_uses_emitter() {
        let known = topics();
        let event = log(
            vec![known.transfer_single, word(1), word(2), word(3)],
            data_words(&[4, 5]),
        );
        match &classify(1, &event)[..] {
            [DiscoveryHit::Asset {
                standard: AssetKind::Erc1155,
                asset,
                ..
            }] => assert_eq!(*asset, event.address),
            other => panic!("expected erc1155, got {other:?}"),
        }
    }

    #[test]
    fn pair_created_emits_venue_and_token_hints() {
        let known = topics();
        let event = log(
            vec![known.pair_created, word(0x0a), word(0x0b)],
            data_words(&[0x0c, 1]),
        );
        let hits = classify(1, &event);
        assert!(matches!(
            &hits[0],
            DiscoveryHit::Venue {
                venue: VenueKind::UniswapV2Pair,
                id: VenueId::Address(pair),
                assets,
                ..
            } if *pair == Address::from_word(word(0x0c)) && assets.len() == 2
        ));
        assert!(matches!(
            &hits[1],
            DiscoveryHit::Asset {
                standard: AssetKind::Unclassified,
                ..
            }
        ));
        assert_eq!(hits.len(), 3);
    }

    #[test]
    fn pool_created_reads_pool_from_second_data_word() {
        let known = topics();
        let event = log(
            vec![known.pool_created, word(1), word(2), word(3)],
            data_words(&[9, 0x2a]),
        );
        let hits = classify(1, &event);
        assert!(matches!(
            &hits[0],
            DiscoveryHit::Venue {
                venue: VenueKind::UniswapV3Pool,
                id: VenueId::Address(pool),
                ..
            } if *pool == Address::from_word(word(0x2a))
        ));
        let pool = univ3_pool(&event).expect("pool");
        assert_eq!(pool.fee, 3);
        assert_eq!(pool.tick_spacing, 9);
        assert_eq!(pool.pool, Address::from_word(word(0x2a)));
    }

    #[test]
    fn pool_created_keeps_negative_tick_spacing() {
        let known = topics();
        let mut data = vec![0_u8; 64];
        data[..32].fill(0xff);
        data[31] = 0xc4; // int24 -60
        data[63] = 1;
        let event = log(
            vec![known.pool_created, word(1), word(2), word_u32(3000)],
            Bytes::from(data),
        );
        let pool = univ3_pool(&event).expect("pool");
        assert_eq!(pool.fee, 3000);
        assert_eq!(pool.tick_spacing, -60);
    }

    #[test]
    fn v4_initialize_uses_pool_id() {
        let known = topics();
        let pool_id = word(0x44);
        let event = log(
            vec![known.v4_initialize, pool_id, word(1), word(2)],
            Bytes::new(),
        );
        let hits = classify(1, &event);
        assert!(matches!(
            &hits[0],
            DiscoveryHit::Venue {
                venue: VenueKind::UniswapV4Pool,
                id: VenueId::PoolId(id),
                ..
            } if *id == pool_id
        ));
    }

    #[test]
    fn unknown_and_truncated_logs_emit_nothing() {
        let known = topics();
        assert!(classify(1, &log(vec![word(1)], Bytes::new())).is_empty());
        assert!(classify(1, &log(vec![], Bytes::new())).is_empty());
        let short = log(vec![known.pair_created, word(1), word(2)], bytes!("01"));
        assert!(classify(1, &short).is_empty());
    }
}
