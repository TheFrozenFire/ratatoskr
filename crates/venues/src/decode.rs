//! Creation-event decoders.
//!
//! The factory address selects the kind. Forks share `PairCreated` and
//! `PoolCreated` with Uniswap. Uniswap v4, Balancer, and Metric are accepted
//! only from their canonical contract.

use alloy_primitives::{keccak256, Address, B256};
use discover::univ3_pool;
use model::NormalizedLog;
use std::sync::OnceLock;

use crate::catalog::{
    VenueRow, BALANCER_V2, BALANCER_V3, METRIC, PANCAKE_V2, PANCAKE_V3, SUSHI_V2, SUSHI_V3, UNI_V2,
    UNI_V3, UNI_V4, V2_FORK, V3_FORK,
};

fn bal_v2_pool() -> B256 {
    seed_topic("balancer-v2", 0)
}

fn bal_v2_tokens() -> B256 {
    seed_topic("balancer-v2", 1)
}

fn bal_v3_pool() -> B256 {
    seed_topic("balancer-v3", 0)
}

fn metric_created() -> B256 {
    seed_topic("metric", 0)
}

fn seed_topic(name: &str, index: usize) -> B256 {
    crate::seeds::document().by_name(name).topics[index]
}

struct Topics {
    pair: B256,
}

fn topics() -> &'static Topics {
    static TOPICS: OnceLock<Topics> = OnceLock::new();
    TOPICS.get_or_init(|| Topics {
        pair: keccak256("PairCreated(address,address,address,uint256)"),
    })
}

pub struct Known {
    pub uni_v2: Address,
    pub sushi_v2: Address,
    pub pancake_v2: Address,
    pub uni_v3: Address,
    pub pancake_v3: Address,
    pub sushi_v3: Address,
    #[cfg_attr(not(test), allow(dead_code))]
    pub aero: Address,
    pub pool_manager: Address,
    pub bal_v2: Address,
    pub bal_v3: Address,
    pub metric: Address,
    #[cfg_attr(not(test), allow(dead_code))]
    pub raw_v3_a: Address,
    #[cfg_attr(not(test), allow(dead_code))]
    pub raw_v3_b: Address,
}

pub fn known() -> &'static Known {
    static KNOWN: OnceLock<Known> = OnceLock::new();
    KNOWN.get_or_init(|| {
        let doc = crate::seeds::document();
        let mut forks = doc
            .roots
            .iter()
            .filter(|root| root.kind == V3_FORK && !root.registries);
        Known {
            uni_v2: doc.by_name("uni-v2").address,
            sushi_v2: doc.by_name("sushi-v2").address,
            pancake_v2: doc.by_name("pancake-v2").address,
            uni_v3: doc.by_name("uni-v3").address,
            pancake_v3: doc.by_name("pancake-v3").address,
            sushi_v3: doc.by_name("sushi-v3").address,
            aero: doc.by_name("aero").address,
            pool_manager: doc.by_name("uni-v4").address,
            bal_v2: doc.by_name("balancer-v2").address,
            bal_v3: doc.by_name("balancer-v3").address,
            metric: doc.by_name("metric").address,
            raw_v3_a: forks.next().expect("v3 fork").address,
            raw_v3_b: forks.next().expect("v3 fork").address,
        }
    })
}

pub fn v2_kind(factory: Address) -> u8 {
    let known = known();
    if factory == known.uni_v2 {
        UNI_V2
    } else if factory == known.sushi_v2 {
        SUSHI_V2
    } else if factory == known.pancake_v2 {
        PANCAKE_V2
    } else {
        V2_FORK
    }
}

pub fn v3_kind(factory: Address) -> u8 {
    let known = known();
    if factory == known.uni_v3 {
        UNI_V3
    } else if factory == known.pancake_v3 {
        PANCAKE_V3
    } else if factory == known.sushi_v3 {
        SUSHI_V3
    } else {
        V3_FORK
    }
}

pub fn topic_pair() -> B256 {
    topics().pair
}

pub fn topic_pool() -> B256 {
    seed_topic("uni-v3", 0)
}

pub fn topic_v4() -> B256 {
    seed_topic("uni-v4", 0)
}

pub fn rows_from_log(log: &NormalizedLog) -> Vec<VenueRow> {
    let Some(&topic0) = log.topics.first() else {
        return Vec::new();
    };
    let known = known();
    if topic0 == topic_pair() {
        return pair_row(log).into_iter().collect();
    }
    if topic0 == topic_pool() {
        return pool_row(log).into_iter().collect();
    }
    if topic0 == topic_v4() && log.address == known.pool_manager {
        return v4_row(log).into_iter().collect();
    }
    if log.address == known.bal_v2 && topic0 == bal_v2_pool() {
        return bal_v2_pool_row(log).into_iter().collect();
    }
    if log.address == known.bal_v2 && topic0 == bal_v2_tokens() {
        return bal_v2_tokens_row(log).into_iter().collect();
    }
    if log.address == known.bal_v3 && topic0 == bal_v3_pool() {
        return bal_v3_row(log).into_iter().collect();
    }
    if log.address == known.metric && topic0 == metric_created() {
        return metric_row(log).into_iter().collect();
    }
    Vec::new()
}

fn pair_row(log: &NormalizedLog) -> Option<VenueRow> {
    if log.topics.len() != 3 {
        return None;
    }
    let pair = address_word(&log.data, 0)?;
    Some(row(
        v2_kind(log.address),
        VenueRow::address_id(pair),
        log.address,
        vec![
            address_from_topic(log.topics[1]),
            address_from_topic(log.topics[2]),
        ],
        None,
        None,
        Some(log.block_number),
        None,
        Some(log.block_hash),
        Some(log.tx_index),
        Some(log.log_index),
    ))
}

fn pool_row(log: &NormalizedLog) -> Option<VenueRow> {
    let pool = univ3_pool(log)?;
    Some(row(
        v3_kind(log.address),
        VenueRow::address_id(pool.pool),
        log.address,
        vec![pool.token0, pool.token1],
        Some(pool.fee),
        Some(pool.tick_spacing),
        Some(log.block_number),
        None,
        Some(log.block_hash),
        Some(log.tx_index),
        Some(log.log_index),
    ))
}

fn v4_row(log: &NormalizedLog) -> Option<VenueRow> {
    if log.topics.len() != 4 {
        return None;
    }
    let fee = u32_word(&log.data, 0);
    let tick_spacing = i32_word(&log.data, 1);
    let hooks = address_word(&log.data, 2).unwrap_or(Address::ZERO);
    Some(row(
        UNI_V4,
        log.topics[1],
        log.address,
        vec![
            address_from_topic(log.topics[2]),
            address_from_topic(log.topics[3]),
        ],
        fee,
        tick_spacing,
        Some(log.block_number),
        Some(hooks),
        Some(log.block_hash),
        Some(log.tx_index),
        Some(log.log_index),
    ))
}

fn bal_v2_pool_row(log: &NormalizedLog) -> Option<VenueRow> {
    if log.topics.len() < 3 {
        return None;
    }
    Some(row(
        BALANCER_V2,
        log.topics[1],
        log.address,
        Vec::new(),
        None,
        None,
        Some(log.block_number),
        Some(address_from_topic(log.topics[2])),
        Some(log.block_hash),
        Some(log.tx_index),
        Some(log.log_index),
    ))
}

fn bal_v2_tokens_row(log: &NormalizedLog) -> Option<VenueRow> {
    if log.topics.len() < 2 {
        return None;
    }
    Some(row(
        BALANCER_V2,
        log.topics[1],
        log.address,
        address_array(&log.data, 0),
        None,
        None,
        Some(log.block_number),
        None,
        Some(log.block_hash),
        Some(log.tx_index),
        Some(log.log_index),
    ))
}

fn bal_v3_row(log: &NormalizedLog) -> Option<VenueRow> {
    if log.topics.len() < 3 {
        return None;
    }
    Some(row(
        BALANCER_V3,
        VenueRow::address_id(address_from_topic(log.topics[1])),
        address_from_topic(log.topics[2]),
        token_configs(&log.data),
        None,
        None,
        Some(log.block_number),
        None,
        Some(log.block_hash),
        Some(log.tx_index),
        Some(log.log_index),
    ))
}

fn metric_row(log: &NormalizedLog) -> Option<VenueRow> {
    if log.topics.len() < 4 {
        return None;
    }
    let factory = address_word(&log.data, 1).unwrap_or(log.address);
    Some(row(
        METRIC,
        VenueRow::address_id(address_from_topic(log.topics[1])),
        factory,
        vec![
            address_from_topic(log.topics[2]),
            address_from_topic(log.topics[3]),
        ],
        None,
        None,
        Some(log.block_number),
        None,
        Some(log.block_hash),
        Some(log.tx_index),
        Some(log.log_index),
    ))
}

#[allow(clippy::too_many_arguments)]
fn row(
    kind: u8,
    id: B256,
    factory: Address,
    tokens: Vec<Address>,
    fee: Option<u32>,
    tick_spacing: Option<i32>,
    block_number: Option<u64>,
    extra: Option<Address>,
    block_hash: Option<B256>,
    tx_index: Option<u64>,
    log_index: Option<u64>,
) -> VenueRow {
    VenueRow {
        kind,
        id,
        factory,
        tokens,
        fee,
        tick_spacing,
        block_number,
        extra,
        block_hash,
        tx_index,
        log_index,
    }
}

fn address_from_topic(word: B256) -> Address {
    Address::from_slice(&word.as_slice()[12..])
}

fn word(data: &[u8], index: usize) -> Option<&[u8]> {
    let start = index.checked_mul(32)?;
    data.get(start..start + 32)
}

fn address_word(data: &[u8], index: usize) -> Option<Address> {
    Some(Address::from_slice(&word(data, index)?[12..]))
}

fn u32_word(data: &[u8], index: usize) -> Option<u32> {
    let word = word(data, index)?;
    if word[..28].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(u32::from_be_bytes(word[28..32].try_into().ok()?))
}

fn i32_word(data: &[u8], index: usize) -> Option<i32> {
    let word = word(data, index)?;
    let negative = word[0] >= 0x80;
    let fill = if negative { 0xff } else { 0x00 };
    if word[..28].iter().any(|byte| *byte != fill) {
        return None;
    }
    Some(i32::from_be_bytes(word[28..32].try_into().ok()?))
}

fn word_usize(data: &[u8], index: usize) -> Option<usize> {
    let word = word(data, index)?;
    if word[..24].iter().any(|byte| *byte != 0) {
        return None;
    }
    usize::try_from(u64::from_be_bytes(word[24..32].try_into().ok()?)).ok()
}

/// First dynamic `address[]` at the ABI offset stored in `head`.
fn address_array(data: &[u8], head: usize) -> Vec<Address> {
    let Some(offset) = word_usize(data, head) else {
        return Vec::new();
    };
    if offset % 32 != 0 {
        return Vec::new();
    }
    let Some(len) = word_usize(data, offset / 32) else {
        return Vec::new();
    };
    let mut tokens = Vec::new();
    for index in 0..len.min(16) {
        let Some(address) = address_word(data, offset / 32 + 1 + index) else {
            break;
        };
        tokens.push(address);
    }
    tokens
}

/// Balancer v3 `TokenConfig[]`. Word 0 is the byte offset. Each tuple is four words.
fn token_configs(data: &[u8]) -> Vec<Address> {
    let Some(offset) = word_usize(data, 0) else {
        return Vec::new();
    };
    if offset % 32 != 0 {
        return Vec::new();
    }
    let base = offset / 32;
    let Some(len) = word_usize(data, base) else {
        return Vec::new();
    };
    let mut tokens = Vec::new();
    for index in 0..len.min(16) {
        let Some(address) = address_word(data, base + 1 + index * 4) else {
            break;
        };
        tokens.push(address);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{b256, Address, Bytes};

    fn address(text: &str) -> Address {
        let text = if text.starts_with("0x") {
            text.to_string()
        } else {
            format!("0x{text}")
        };
        text.parse().unwrap()
    }

    fn bare(address: Address, topics: Vec<B256>, data: Vec<u8>) -> NormalizedLog {
        NormalizedLog {
            block_number: 100,
            block_hash: B256::ZERO,
            tx_index: 0,
            tx_hash: None,
            log_index: 1,
            address,
            topics,
            data: Bytes::from(data),
        }
    }

    fn be_word(value: u64) -> [u8; 32] {
        let mut word = [0u8; 32];
        word[24..].copy_from_slice(&value.to_be_bytes());
        word
    }

    fn addr_word(address: Address) -> [u8; 32] {
        let mut word = [0u8; 32];
        word[12..].copy_from_slice(address.as_slice());
        word
    }

    #[test]
    fn balancer_v2_tokens_follow_the_pool_id() {
        let id = b256!("1111111111111111111111111111111111111111111111111111111111111111");
        let pool = address("2222222222222222222222222222222222222222");
        let token_a = address("3333333333333333333333333333333333333333");
        let token_b = address("4444444444444444444444444444444444444444");
        let registered = bare(
            known().bal_v2,
            vec![bal_v2_pool(), id, addr_word(pool).into()],
            be_word(1).to_vec(),
        );
        let row = rows_from_log(&registered);
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].id, id);
        assert_eq!(row[0].extra, Some(pool));
        assert!(row[0].tokens.is_empty());

        let mut data = Vec::new();
        data.extend_from_slice(&be_word(0x40));
        data.extend_from_slice(&be_word(0xa0));
        data.extend_from_slice(&be_word(2));
        data.extend_from_slice(&addr_word(token_a));
        data.extend_from_slice(&addr_word(token_b));
        data.extend_from_slice(&be_word(2));
        data.extend_from_slice(&[0u8; 64]);
        let tokens = bare(known().bal_v2, vec![bal_v2_tokens(), id], data);
        let row = rows_from_log(&tokens);
        assert_eq!(row[0].tokens, vec![token_a, token_b]);
        assert_eq!(row[0].id, id);
        assert!(row[0].extra.is_none());
    }

    #[test]
    fn balancer_v3_token_config_stride_is_four_words() {
        let pool = address("527d0e14acc53fb040debeae1cab973d23fb3568");
        let factory = address("201efd508c8dfe9de1a13c2452863a78cb2a86cc");
        let bal = address("ba100000625a3754423978a60c9317c58a424e3d");
        let weth = address("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let mut data = vec![0u8; 32 * 30];
        data[..32].copy_from_slice(&be_word(0x2a0));
        let base = 0x2a0 / 32;
        data[base * 32..(base + 1) * 32].copy_from_slice(&be_word(2));
        data[(base + 1) * 32..(base + 2) * 32].copy_from_slice(&addr_word(bal));
        data[(base + 5) * 32..(base + 6) * 32].copy_from_slice(&addr_word(weth));
        let log = bare(
            known().bal_v3,
            vec![
                bal_v3_pool(),
                addr_word(pool).into(),
                addr_word(factory).into(),
            ],
            data,
        );
        let row = rows_from_log(&log);
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].kind, BALANCER_V3);
        assert_eq!(row[0].factory, factory);
        assert_eq!(row[0].id, VenueRow::address_id(pool));
        assert_eq!(row[0].tokens, vec![bal, weth]);
    }

    #[test]
    fn v4_initialize_keeps_hooks_and_dynamic_fee() {
        let id = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let c0 = address("0000000000000000000000000000000000000000");
        let c1 = address("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let hooks = address("5555555555555555555555555555555555555555");
        let mut data = Vec::new();
        data.extend_from_slice(&be_word(0x800000));
        data.extend_from_slice(&be_word(60));
        data.extend_from_slice(&addr_word(hooks));
        data.extend_from_slice(&be_word(1));
        let mut tick = [0xff_u8; 32];
        tick[31] = 0xf6;
        data.extend_from_slice(&tick);
        let log = bare(
            known().pool_manager,
            vec![topic_v4(), id, addr_word(c0).into(), addr_word(c1).into()],
            data,
        );
        let row = rows_from_log(&log);
        assert_eq!(row[0].kind, UNI_V4);
        assert_eq!(row[0].id, id);
        assert_eq!(row[0].tokens, vec![c0, c1]);
        assert_eq!(row[0].fee, Some(0x800000));
        assert_eq!(row[0].tick_spacing, Some(60));
        assert_eq!(row[0].extra, Some(hooks));
    }

    #[test]
    fn metric_topics_are_the_pool_and_tokens() {
        let pool = address("f85afbadecc7f23dd173ed706f07d9c7e2473e24");
        let token0 = address("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2");
        let token1 = address("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48");
        let mut data = Vec::new();
        data.extend_from_slice(&be_word(1));
        data.extend_from_slice(&addr_word(known().metric));
        let log = bare(
            known().metric,
            vec![
                metric_created(),
                addr_word(pool).into(),
                addr_word(token0).into(),
                addr_word(token1).into(),
            ],
            data,
        );
        let row = rows_from_log(&log);
        assert_eq!(row[0].kind, METRIC);
        assert_eq!(row[0].tokens, vec![token0, token1]);
        assert_eq!(row[0].factory, known().metric);
        assert_eq!(row[0].id, VenueRow::address_id(pool));
    }

    #[test]
    fn pair_created_uses_the_first_data_word() {
        let token0 = address("1111111111111111111111111111111111111111");
        let token1 = address("2222222222222222222222222222222222222222");
        let pair = address("3333333333333333333333333333333333333333");
        let log = bare(
            known().uni_v2,
            vec![
                topic_pair(),
                addr_word(token0).into(),
                addr_word(token1).into(),
            ],
            addr_word(pair).to_vec(),
        );
        let row = rows_from_log(&log);
        assert_eq!(row[0].kind, UNI_V2);
        assert_eq!(row[0].id, VenueRow::address_id(pair));
        assert_eq!(row[0].tokens, vec![token0, token1]);
    }

    #[test]
    fn other_emitters_of_pool_created_are_forks() {
        let token0 = address("1111111111111111111111111111111111111111");
        let token1 = address("2222222222222222222222222222222222222222");
        let pool = address("3333333333333333333333333333333333333333");
        let mut fee = B256::ZERO;
        fee.0[30..].copy_from_slice(&3000u16.to_be_bytes());
        let mut data = Vec::new();
        data.extend_from_slice(&be_word(60));
        data.extend_from_slice(&addr_word(pool));
        let mut log = bare(
            known().raw_v3_a,
            vec![
                topic_pool(),
                addr_word(token0).into(),
                addr_word(token1).into(),
                fee,
            ],
            data,
        );
        log.block_hash = b256!("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef");
        log.tx_index = 4;
        log.log_index = 8;
        let row = rows_from_log(&log);
        assert_eq!(
            known().aero,
            crate::seeds::document().by_name("aero").address
        );
        assert_ne!(known().raw_v3_a, known().raw_v3_b);
        assert_eq!(row[0].kind, V3_FORK);
        assert_eq!(row[0].fee, Some(3000));
        assert_eq!(row[0].tick_spacing, Some(60));
        assert_eq!(row[0].block_hash, Some(log.block_hash));
        assert_eq!(row[0].tx_index, Some(log.tx_index));
        assert_eq!(row[0].log_index, Some(log.log_index));
    }
}
