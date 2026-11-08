//! Creation-event decoders.
//!
//! The factory address selects the kind. Forks share `PairCreated` and
//! `PoolCreated` with Uniswap. Uniswap v4, Balancer, and Metric are accepted
//! only from their canonical contract.

use alloy_primitives::{b256, keccak256, Address, B256};
use discover::univ3_pool;
use model::NormalizedLog;
use std::sync::OnceLock;

use crate::catalog::{
    VenueRow, BALANCER_V2, BALANCER_V3, METRIC, PANCAKE_V2, PANCAKE_V3, SUSHI_V2, SUSHI_V3, UNI_V2,
    UNI_V3, UNI_V4, V2_FORK, V3_FORK,
};

pub const BAL_V2_POOL: B256 =
    b256!("3c13bc30b8e878c53fd2a36b679409c073afd75950be43d8858768e956fbc20e");
pub const BAL_V2_TOKENS: B256 =
    b256!("f5847d3f2197b16cdcd2098ec95d0905cd1abdaf415f07bb7cef2bba8ac5dec4");
pub const BAL_V3_POOL: B256 =
    b256!("bc1561eeab9f40962e2fb827a7ff9c7cdb47a9d7c84caeefa4ed90e043842dad");
/// Observed Metric creation topic. topic1 is the pool, topic2 and topic3 the tokens.
/// The signature string was not recovered.
pub const METRIC_CREATED: B256 =
    b256!("4b36a0ddce54edb36597ee7d496df06c53fe875aba9d7257534a38d5177899aa");

struct Topics {
    pair: B256,
    pool: B256,
    v4: B256,
}

fn topics() -> &'static Topics {
    static TOPICS: OnceLock<Topics> = OnceLock::new();
    TOPICS.get_or_init(|| Topics {
        pair: keccak256("PairCreated(address,address,address,uint256)"),
        pool: keccak256("PoolCreated(address,address,uint24,int24,address)"),
        v4: keccak256("Initialize(bytes32,address,address,uint24,int24,address,uint160,int24)"),
    })
}

pub struct Known {
    pub uni_v2: Address,
    pub sushi_v2: Address,
    pub pancake_v2: Address,
    pub uni_v3: Address,
    pub pancake_v3: Address,
    pub sushi_v3: Address,
    pub aero: Address,
    pub pool_manager: Address,
    pub bal_v2: Address,
    pub bal_v3: Address,
    pub metric: Address,
    pub raw_v3_a: Address,
    pub raw_v3_b: Address,
}

pub fn known() -> &'static Known {
    static KNOWN: OnceLock<Known> = OnceLock::new();
    KNOWN.get_or_init(|| Known {
        uni_v2: parse("0x5c69bee701ef814a2b6a3edd4b1652cb9cc5aa6f"),
        sushi_v2: parse("0xc0aee478e3658e2610c5f7a4a2e1777ce9e4f2ac"),
        pancake_v2: parse("0x1097053fd2ea711dad45caccc45eff7548fcb362"),
        uni_v3: parse("0x1f98431c8ad98523631ae4a59f267346ea31f984"),
        pancake_v3: parse("0x0bfbcf9fa4f9c56b0f40a671ad40e0805a091865"),
        sushi_v3: parse("0xbaceb8ec6b9355dfc0269c18bac9d6e2bdc29c4f"),
        aero: parse("0xf4fd2e6c5b60c5b365cd1b96dd6245e6d60414c2"),
        pool_manager: parse("0x000000000004444c5dc75cb358380d2e3de08a90"),
        bal_v2: parse("0xba12222222228d8ba445958a75a0704d566bf2c8"),
        bal_v3: parse("0xba1333333333a1ba1108e8412f11850a5c319ba9"),
        metric: parse("0x2a53833cc95548cf52c7b159110e22d3a9018f32"),
        raw_v3_a: parse("0x88695a239a0a3f70729c4cb30fd32f9b4978513d"),
        raw_v3_b: parse("0x956920f97690bdd76a6a10c7ba931dec14f61169"),
    })
}

fn parse(text: &str) -> Address {
    text.parse().unwrap_or_else(|err| panic!("{text}: {err}"))
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
    topics().pool
}

pub fn topic_v4() -> B256 {
    topics().v4
}

pub fn rows_from_log(log: &NormalizedLog) -> Vec<VenueRow> {
    let Some(&topic0) = log.topics.first() else {
        return Vec::new();
    };
    let known = known();
    let topics = topics();
    if topic0 == topic_pair() {
        return pair_row(log).into_iter().collect();
    }
    if topic0 == topics.pool {
        return pool_row(log).into_iter().collect();
    }
    if topic0 == topics.v4 && log.address == known.pool_manager {
        return v4_row(log).into_iter().collect();
    }
    if log.address == known.bal_v2 && topic0 == BAL_V2_POOL {
        return bal_v2_pool(log).into_iter().collect();
    }
    if log.address == known.bal_v2 && topic0 == BAL_V2_TOKENS {
        return bal_v2_tokens(log).into_iter().collect();
    }
    if log.address == known.bal_v3 && topic0 == BAL_V3_POOL {
        return bal_v3_row(log).into_iter().collect();
    }
    if log.address == known.metric && topic0 == METRIC_CREATED {
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
    ))
}

fn bal_v2_pool(log: &NormalizedLog) -> Option<VenueRow> {
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
    ))
}

fn bal_v2_tokens(log: &NormalizedLog) -> Option<VenueRow> {
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
    ))
}

fn row(
    kind: u8,
    id: B256,
    factory: Address,
    tokens: Vec<Address>,
    fee: Option<u32>,
    tick_spacing: Option<i32>,
    block_number: Option<u64>,
    extra: Option<Address>,
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
    }
}

fn address_from_topic(word: B256) -> Address {
    Address::from_slice(&word.as_slice()[12..])
}

fn word<'a>(data: &'a [u8], index: usize) -> Option<&'a [u8]> {
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
            vec![BAL_V2_POOL, id, addr_word(pool).into()],
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
        let tokens = bare(known().bal_v2, vec![BAL_V2_TOKENS, id], data);
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
                BAL_V3_POOL,
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
                METRIC_CREATED,
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
        let log = bare(
            known().raw_v3_a,
            vec![
                topic_pool(),
                addr_word(token0).into(),
                addr_word(token1).into(),
                fee,
            ],
            data,
        );
        let row = rows_from_log(&log);
        assert_eq!(row[0].kind, V3_FORK);
        assert_eq!(row[0].fee, Some(3000));
        assert_eq!(row[0].tick_spacing, Some(60));
    }
}
