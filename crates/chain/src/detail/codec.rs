//! Detail lens words.

use super::types::*;
use alloy_primitives::{Address, B256, I256, U256};
use store_model::{
    AERO, BALANCER_V2, BALANCER_V3, CURVE, METRIC, PANCAKE_V2, PANCAKE_V3, SUSHI_V2, SUSHI_V3,
    UNI_V2, UNI_V3, UNI_V4, V2_FORK, V3_FORK,
};

pub(crate) const MAGIC: [u8; 32] = [
    0x9a, 0x7a, 0xd5, 0x55, 0x5f, 0xe5, 0x50, 0xf2, 0x7c, 0x28, 0x58, 0x1e, 0x7e, 0xf0, 0xa5, 0x54,
    0xb2, 0x1e, 0x5f, 0xbb, 0x38, 0x78, 0x33, 0x3e, 0xe6, 0x1e, 0x72, 0x1f, 0x17, 0x05, 0x53, 0xaa,
];
pub(crate) const LENS_BATCH: usize = 16;

pub fn decode_details(bytes: &[u8], queries: &[DetailQuery]) -> Result<Vec<PoolDetail>, String> {
    if bytes.len() < 64 || !bytes.len().is_multiple_of(32) || bytes[..32] != MAGIC {
        return Err("detail payload has no magic".into());
    }
    let count = read_u64(&bytes[32..64])?;
    if count as usize != queries.len() || count > LENS_BATCH as u64 {
        return Err(format!("detail count {count}"));
    }
    let mut at = 64;
    let mut rows = Vec::with_capacity(count as usize);
    for query in queries {
        if at + 128 > bytes.len() {
            return Err("detail record header".into());
        }
        let n_words = read_u64(&bytes[at..at + 32])?;
        if n_words < 4 {
            return Err("detail record length".into());
        }
        let end = at
            .checked_add(n_words as usize * 32)
            .ok_or_else(|| "detail record length".to_string())?;
        if end > bytes.len() {
            return Err("detail record length".into());
        }
        let kind = read_u64(&bytes[at + 32..at + 64])?;
        if kind > u64::from(u8::MAX) {
            return Err("detail kind".into());
        }
        let id = B256::from_slice(&bytes[at + 64..at + 96]);
        let flags = read_u64(&bytes[at + 96..at + 128])?;
        if kind as u8 != query.kind || id != query.id {
            return Err("detail record does not match the query".into());
        }
        let body = read_words(&bytes[at + 128..end])?;
        let mut detail = interpret(kind as u8, id, flags, &body)?;
        stamp(&mut detail, query);
        rows.push(detail);
        at = end;
    }
    if at != bytes.len() {
        return Err("detail trailing bytes".into());
    }
    Ok(rows)
}

fn interpret(kind: u8, id: B256, flags: u64, body: &[U256]) -> Result<PoolDetail, String> {
    let detail = match kind {
        UNI_V2 | SUSHI_V2 | PANCAKE_V2 | V2_FORK => {
            constant_product(kind, id, flags, body).map(PoolDetail::ConstantProduct)
        }
        UNI_V3 | PANCAKE_V3 | SUSHI_V3 | V3_FORK | AERO => {
            concentrated(false, kind, id, flags, body).map(PoolDetail::Concentrated)
        }
        UNI_V4 => concentrated(true, kind, id, flags, body).map(PoolDetail::Concentrated),
        BALANCER_V2 | BALANCER_V3 => balancer(kind, id, flags, body).map(PoolDetail::Balancer),
        CURVE => curve(kind, id, flags, body).map(PoolDetail::Curve),
        METRIC => metric(kind, id, flags, body).map(PoolDetail::Metric),
        _ => Err("detail kind".into()),
    };
    Ok(detail.unwrap_or(PoolDetail::Failed(FailedRead {
        kind,
        id,
        home: Address::ZERO,
        flags,
    })))
}

fn constant_product(
    kind: u8,
    id: B256,
    flags: u64,
    body: &[U256],
) -> Result<ConstantProduct, String> {
    if body.len() != 6 {
        return Err("product body".into());
    }
    Ok(ConstantProduct {
        kind,
        id,
        pair: Address::ZERO,
        token0: address_of_word(body[0]),
        token1: address_of_word(body[1]),
        reserve0: body[2],
        reserve1: body[3],
        fee_pips: fit_u32(body[4]),
        timestamp: fit_u64(body[5]).unwrap_or(0),
        flags,
    })
}

fn concentrated(
    v4: bool,
    kind: u8,
    id: B256,
    flags: u64,
    body: &[U256],
) -> Result<Concentrated, String> {
    let count_at = if v4 { 10 } else { 8 };
    if body.len() <= count_at {
        return Err("concentrated body".into());
    }
    let count = fit_u64(body[count_at]).ok_or_else(|| "bitmap count".to_string())? as usize;
    if count > 4096 || body.len() != count_at + 1 + count * 2 {
        return Err("bitmap count".into());
    }
    let mut bitmaps = Vec::with_capacity(count);
    for index in 0..count {
        let word_pos =
            fit_i32(body[count_at + 1 + index * 2]).ok_or_else(|| "word pos".to_string())?;
        let word_pos = i16::try_from(word_pos).map_err(|_| "word pos")?;
        bitmaps.push(BitmapWord {
            word_pos,
            bits: body[count_at + 2 + index * 2],
        });
    }
    let (hooks, fee_at, spacing_at, sqrt_at, tick_at, liq_at, protocol_at, next_at) = if v4 {
        (address_of_word(body[2]), 3, 4, 5, 6, 7, Some(8), 9)
    } else {
        (Address::ZERO, 2, 3, 4, 5, 6, None, 7)
    };
    Ok(Concentrated {
        kind,
        id,
        home: Address::ZERO,
        hooks,
        token0: address_of_word(body[0]),
        token1: address_of_word(body[1]),
        fee_pips: fit_u32(body[fee_at]),
        protocol_fee: protocol_at.and_then(|at| fit_u32(body[at])).unwrap_or(0),
        tick_spacing: fit_i32(body[spacing_at]).ok_or_else(|| "tick spacing".to_string())?,
        sqrt_price_x96: body[sqrt_at],
        tick: fit_i32(body[tick_at]).ok_or_else(|| "tick".to_string())?,
        liquidity: fit_u128(body[liq_at]).ok_or_else(|| "liquidity".to_string())?,
        next_scan: optional_i32(body[next_at])?,
        bitmaps,
        ticks: Vec::new(),
        flags,
    })
}

fn balancer(kind: u8, id: B256, flags: u64, body: &[U256]) -> Result<BalancerPool, String> {
    if body.len() < 5 {
        return Err("balancer body".into());
    }
    let n = fit_u64(body[1]).ok_or_else(|| "balancer n".to_string())? as usize;
    let weights = fit_u64(body[4]).ok_or_else(|| "balancer weights".to_string())? as usize;
    let base = 5 + n * 2 + weights;
    if n > 8 || weights > 8 || (body.len() != base && body.len() != base + n * 2) {
        return Err("balancer body".into());
    }
    let mut tokens = Vec::with_capacity(n);
    let mut balances = Vec::with_capacity(n);
    for index in 0..n {
        tokens.push(address_of_word(body[5 + index * 2]));
        balances.push(body[6 + index * 2]);
    }
    let (scaling, rates) = if body.len() == base {
        (Vec::new(), Vec::new())
    } else {
        let at = base;
        (body[at..at + n].to_vec(), body[at + n..at + n * 2].to_vec())
    };
    Ok(BalancerPool {
        kind,
        id,
        vault: Address::ZERO,
        pool: Address::ZERO,
        fee: optional_u256(body[0]),
        amp: body[2],
        amp_precision: body[3],
        tokens,
        balances,
        weights: body[5 + n * 2..5 + n * 2 + weights].to_vec(),
        scaling,
        rates,
        flags,
    })
}

fn curve(kind: u8, id: B256, flags: u64, body: &[U256]) -> Result<CurvePool, String> {
    if body.len() < 9 {
        return Err("curve body".into());
    }
    let n = fit_u64(body[0]).ok_or_else(|| "curve n".to_string())? as usize;
    let base = 9 + n * 4;
    if n > 8 || (body.len() != base && body.len() != base + n) {
        return Err("curve body".into());
    }
    let mut coins = Vec::with_capacity(n);
    for index in 0..n {
        let at = 9 + index * 4;
        coins.push(CurveCoin {
            token: address_of_word(body[at]),
            balance: body[at + 1],
            rate: body[at + 2],
            price_scale: body[at + 3],
            precision: if body.len() == base + n {
                body[base + index]
            } else {
                U256::ZERO
            },
        });
    }
    Ok(CurvePool {
        kind,
        id,
        pool: Address::ZERO,
        amp: body[1],
        fee: optional_u256(body[2]),
        gamma: body[3],
        d: body[4],
        mid_fee: body[5],
        out_fee: body[6],
        fee_gamma: body[7],
        coins_indexed_by_uint: !body[8].is_zero(),
        coins,
        flags,
    })
}

fn metric(kind: u8, id: B256, flags: u64, body: &[U256]) -> Result<MetricPool, String> {
    if body.len() < 7 {
        return Err("metric body".into());
    }
    let slot0 = decode_slot0(body[0]);
    let (next_scan, bins) = if body.len() == 7 {
        (None, Vec::new())
    } else {
        if body.len() < 9 {
            return Err("metric bins".into());
        }
        let n = fit_u64(body[8]).ok_or_else(|| "metric bins".to_string())? as usize;
        if body.len() != 9 + n * 2 {
            return Err("metric bins".into());
        }
        let mut bins = Vec::with_capacity(n);
        for index in 0..n {
            let bin = fit_i32(body[9 + index * 2]).ok_or_else(|| "metric bin".to_string())?;
            bins.push(decode_bin(bin, body[10 + index * 2]));
        }
        (optional_i32(body[7])?, bins)
    };
    Ok(MetricPool {
        kind,
        id,
        pool: Address::ZERO,
        pause_level: slot0.0,
        bin_index: slot0.1,
        position: slot0.2,
        distance_e6: slot0.3,
        spread_fee_e6: slot0.4,
        notional_fee_e8: slot0.5,
        asymmetry_x8: slot0.6,
        scale0: body[1],
        scale1: body[2],
        lowest_bin: fit_i32(body[3]).ok_or_else(|| "metric lowest".to_string())?,
        highest_bin: fit_i32(body[4]).ok_or_else(|| "metric highest".to_string())?,
        token0: Address::ZERO,
        token1: Address::ZERO,
        reference_x64: body[5],
        provider: address_of_word(body[6]),
        next_scan,
        bins,
        flags,
    })
}

fn stamp(detail: &mut PoolDetail, query: &DetailQuery) {
    match detail {
        PoolDetail::ConstantProduct(pool) => pool.pair = query.home,
        PoolDetail::Concentrated(pool) => pool.home = query.home,
        PoolDetail::Balancer(pool) => {
            if query.kind == BALANCER_V3 {
                pool.pool = query.home;
                pool.vault = query.aux;
            } else {
                pool.vault = query.home;
                pool.pool = query.aux;
            }
        }
        PoolDetail::Curve(pool) => pool.pool = query.home,
        PoolDetail::Metric(pool) => {
            pool.pool = query.home;
            pool.token0 = query.token0;
            pool.token1 = query.token1;
        }
        PoolDetail::Failed(pool) => pool.home = query.home,
    }
}

fn decode_slot0(word: U256) -> (u8, i32, u128, i32, u32, u32, u16) {
    let pause = mask(word, 0, 8).to::<u8>();
    let bin = sign_bits(word, 8, 16);
    let pos = mask(word, 24, 104).to::<u128>();
    let distance = sign_bits(word, 128, 24);
    let spread = mask(word, 152, 24).to::<u32>();
    let notional = mask(word, 176, 24).to::<u32>();
    let asymmetry = mask(word, 200, 16).to::<u16>();
    (pause, bin, pos, distance, spread, notional, asymmetry)
}

fn decode_bin(index: i32, word: U256) -> MetricBin {
    MetricBin {
        index,
        token0_scaled: mask(word, 0, 104).to::<u128>(),
        token1_scaled: mask(word, 104, 104).to::<u128>(),
        length_e6: mask(word, 208, 16).to::<u16>(),
        add_fee_buy_e6: mask(word, 224, 16).to::<u16>(),
        add_fee_sell_e6: mask(word, 240, 16).to::<u16>(),
    }
}

fn mask(word: U256, shift: usize, width: usize) -> U256 {
    (word >> shift) & ((U256::from(1u8) << width) - U256::from(1u8))
}

fn sign_bits(word: U256, shift: usize, width: usize) -> i32 {
    let value = mask(word, shift, width);
    let sign = U256::from(1u8) << (width - 1);
    if value & sign == U256::ZERO {
        value.to::<i32>()
    } else {
        let full = (U256::from(1u8) << width) - value;
        -(full.to::<i32>())
    }
}

fn address_of_word(word: U256) -> Address {
    Address::from_slice(&word.to_be_bytes::<32>()[12..])
}

pub(crate) fn scan_all() -> [u8; 32] {
    let mut bytes = [0xff; 32];
    bytes[0] = 0x7f;
    bytes
}

fn read_u64(word: &[u8]) -> Result<u64, String> {
    if word.len() != 32 || word[..24].iter().any(|byte| *byte != 0) {
        return Err("word".into());
    }
    Ok(u64::from_be_bytes(word[24..32].try_into().unwrap()))
}

fn read_words(bytes: &[u8]) -> Result<Vec<U256>, String> {
    if !bytes.len().is_multiple_of(32) {
        return Err("words".into());
    }
    Ok(bytes
        .chunks(32)
        .map(|word| U256::from_be_slice(word))
        .collect())
}

fn fit_u32(word: U256) -> Option<u32> {
    if word == U256::MAX {
        return None;
    }
    u32::try_from(word).ok()
}

fn fit_u64(word: U256) -> Option<u64> {
    u64::try_from(word).ok()
}

fn fit_u128(word: U256) -> Option<u128> {
    u128::try_from(word).ok()
}

fn fit_i32(word: U256) -> Option<i32> {
    i32::try_from(I256::from_be_bytes(word.to_be_bytes::<32>())).ok()
}

fn optional_i32(word: U256) -> Result<Option<i32>, String> {
    if word.to_be_bytes::<32>() == scan_all() {
        return Ok(None);
    }
    fit_i32(word)
        .map(Some)
        .ok_or_else(|| "next scan".to_string())
}

fn optional_u256(word: U256) -> Option<U256> {
    if word == U256::MAX {
        None
    } else {
        Some(word)
    }
}
