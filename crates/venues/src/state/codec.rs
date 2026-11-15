//! Row bytes for a pool detail.

use crate::detail::{BitmapWord, Concentrated, CurveCoin, CurvePool, MetricBin, MetricPool};
use alloy_primitives::{Address, B256, U256};

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn rest(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.rest() < len {
            return Err("short".into());
        }
        let start = self.at;
        self.at += len;
        Ok(&self.bytes[start..self.at])
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u128(&mut self) -> Result<u128, String> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    fn addr(&mut self) -> Result<Address, String> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn hash(&mut self) -> Result<B256, String> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn word(&mut self) -> Result<U256, String> {
        Ok(U256::from_be_slice(self.take(32)?))
    }

    fn finish(&self) -> Result<(), String> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err("trailing".into())
        }
    }
}

pub(super) fn push_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

pub(super) fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn push_u128(out: &mut Vec<u8>, value: u128) {
    out.extend_from_slice(&value.to_le_bytes());
}

pub(super) fn push_addr(out: &mut Vec<u8>, value: Address) {
    out.extend_from_slice(value.as_slice());
}

pub(super) fn push_word(out: &mut Vec<u8>, value: U256) {
    out.extend_from_slice(&value.to_be_bytes::<32>());
}

pub(super) fn push_option_u32(out: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(value) => {
            push_u8(out, 1);
            push_u32(out, value);
        }
        None => push_u8(out, 0),
    }
}

pub(super) fn push_option_word(out: &mut Vec<u8>, value: Option<U256>) {
    match value {
        Some(value) => {
            push_u8(out, 1);
            push_word(out, value);
        }
        None => push_u8(out, 0),
    }
}

pub(super) fn encode_product_immutable(pool: &crate::detail::ConstantProduct) -> Vec<u8> {
    let mut out = vec![1];
    push_addr(&mut out, pool.pair);
    push_addr(&mut out, pool.token0);
    push_addr(&mut out, pool.token1);
    out
}

pub(super) fn encode_product_mutable(pool: &crate::detail::ConstantProduct) -> Vec<u8> {
    let mut out = Vec::new();
    push_word(&mut out, pool.reserve0);
    push_word(&mut out, pool.reserve1);
    push_option_u32(&mut out, pool.fee_pips);
    push_u64(&mut out, pool.timestamp);
    push_u64(&mut out, pool.flags);
    out
}

pub(super) fn decode_product(
    immutable: &[u8],
    body: &[u8],
) -> Result<crate::detail::ConstantProduct, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 1 {
        return Err("product".into());
    }
    let pair = header.addr()?;
    let token0 = header.addr()?;
    let token1 = header.addr()?;
    header.finish()?;
    let mut body = Reader::new(body);
    let pool = crate::detail::ConstantProduct {
        kind: 0,
        id: B256::ZERO,
        pair,
        token0,
        token1,
        reserve0: body.word()?,
        reserve1: body.word()?,
        fee_pips: if body.u8()? == 1 {
            Some(body.u32()?)
        } else {
            None
        },
        timestamp: body.u64()?,
        flags: body.u64()?,
    };
    body.finish()?;
    Ok(pool)
}

pub(super) fn encode_concentrated_immutable(pool: &Concentrated) -> Vec<u8> {
    let mut out = vec![2];
    push_u8(&mut out, pool.kind);
    out.extend_from_slice(pool.id.as_slice());
    push_addr(&mut out, pool.home);
    push_addr(&mut out, pool.hooks);
    push_addr(&mut out, pool.token0);
    push_addr(&mut out, pool.token1);
    push_option_u32(&mut out, pool.fee_pips);
    push_i32(&mut out, pool.tick_spacing);
    out
}

pub(super) fn encode_concentrated_mutable(pool: &Concentrated) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, pool.protocol_fee);
    push_word(&mut out, pool.sqrt_price_x96);
    push_i32(&mut out, pool.tick);
    push_u128(&mut out, pool.liquidity);
    push_u64(&mut out, pool.flags);
    match pool.next_scan {
        Some(value) => {
            push_u8(&mut out, 1);
            push_i32(&mut out, value);
        }
        None => push_u8(&mut out, 0),
    }
    push_u32(&mut out, pool.bitmaps.len() as u32);
    for word in &pool.bitmaps {
        push_i32(&mut out, i32::from(word.word_pos));
        push_word(&mut out, word.bits);
    }
    out
}

pub(super) fn decode_concentrated(immutable: &[u8], body: &[u8]) -> Result<Concentrated, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 2 {
        return Err("concentrated".into());
    }
    let kind = header.u8()?;
    let id = header.hash()?;
    let pool = Concentrated {
        kind,
        id,
        home: header.addr()?,
        hooks: header.addr()?,
        token0: header.addr()?,
        token1: header.addr()?,
        fee_pips: if header.u8()? == 1 {
            Some(header.u32()?)
        } else {
            None
        },
        tick_spacing: header.i32()?,
        protocol_fee: 0,
        sqrt_price_x96: U256::ZERO,
        tick: 0,
        liquidity: 0,
        next_scan: None,
        bitmaps: Vec::new(),
        ticks: Vec::new(),
        flags: 0,
    };
    header.finish()?;
    let mut body = Reader::new(body);
    let protocol_fee = body.u32()?;
    let sqrt_price_x96 = body.word()?;
    let tick = body.i32()?;
    let liquidity = body.u128()?;
    let flags = body.u64()?;
    let next_scan = if body.u8()? == 1 {
        Some(body.i32()?)
    } else {
        None
    };
    let count = body.u32()? as usize;
    let mut bitmaps = Vec::with_capacity(count);
    for _ in 0..count {
        let word_pos = i16::try_from(body.i32()?).map_err(|_| "word pos")?;
        bitmaps.push(BitmapWord {
            word_pos,
            bits: body.word()?,
        });
    }
    body.finish()?;
    Ok(Concentrated {
        protocol_fee,
        sqrt_price_x96,
        tick,
        liquidity,
        flags,
        next_scan,
        bitmaps,
        ..pool
    })
}

pub(super) fn encode_balancer_immutable(pool: &crate::detail::BalancerPool) -> Vec<u8> {
    let mut out = vec![3];
    push_u8(&mut out, pool.kind);
    out.extend_from_slice(pool.id.as_slice());
    push_addr(&mut out, pool.vault);
    push_addr(&mut out, pool.pool);
    push_u32(&mut out, pool.tokens.len() as u32);
    for token in &pool.tokens {
        push_addr(&mut out, *token);
    }
    out
}

pub(super) fn encode_balancer_mutable(pool: &crate::detail::BalancerPool) -> Vec<u8> {
    let mut out = Vec::new();
    push_option_word(&mut out, pool.fee);
    push_word(&mut out, pool.amp);
    push_word(&mut out, pool.amp_precision);
    push_u64(&mut out, pool.flags);
    push_u32(&mut out, pool.balances.len() as u32);
    for balance in &pool.balances {
        push_word(&mut out, *balance);
    }
    push_u32(&mut out, pool.weights.len() as u32);
    for weight in &pool.weights {
        push_word(&mut out, *weight);
    }
    if pool.scaling.len() == pool.balances.len()
        && pool.rates.len() == pool.balances.len()
        && !pool.scaling.is_empty()
    {
        for factor in &pool.scaling {
            push_word(&mut out, *factor);
        }
        for rate in &pool.rates {
            push_word(&mut out, *rate);
        }
    }
    out
}

pub(super) fn decode_balancer(
    immutable: &[u8],
    body: &[u8],
) -> Result<crate::detail::BalancerPool, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 3 {
        return Err("balancer".into());
    }
    let kind = header.u8()?;
    let id = header.hash()?;
    let vault = header.addr()?;
    let pool = header.addr()?;
    let n = header.u32()? as usize;
    let mut tokens = Vec::with_capacity(n);
    for _ in 0..n {
        tokens.push(header.addr()?);
    }
    header.finish()?;
    let mut body = Reader::new(body);
    let fee = if body.u8()? == 1 {
        Some(body.word()?)
    } else {
        None
    };
    let amp = body.word()?;
    let amp_precision = body.word()?;
    let flags = body.u64()?;
    let balances_n = body.u32()? as usize;
    let mut balances = Vec::with_capacity(balances_n);
    for _ in 0..balances_n {
        balances.push(body.word()?);
    }
    let weights_n = body.u32()? as usize;
    let mut weights = Vec::with_capacity(weights_n);
    for _ in 0..weights_n {
        weights.push(body.word()?);
    }
    let (scaling, rates) = if body.rest() == 0 {
        (Vec::new(), Vec::new())
    } else if body.rest() == balances.len() * 64 {
        let mut scaling = Vec::with_capacity(balances.len());
        let mut rates = Vec::with_capacity(balances.len());
        for _ in 0..balances.len() {
            scaling.push(body.word()?);
        }
        for _ in 0..balances.len() {
            rates.push(body.word()?);
        }
        (scaling, rates)
    } else {
        return Err("trailing".into());
    };
    body.finish()?;
    Ok(crate::detail::BalancerPool {
        kind,
        id,
        vault,
        pool,
        fee,
        amp,
        amp_precision,
        tokens,
        balances,
        weights,
        scaling,
        rates,
        flags,
    })
}

pub(super) fn encode_curve_immutable(pool: &CurvePool) -> Vec<u8> {
    let mut out = vec![4];
    push_u8(&mut out, pool.kind);
    out.extend_from_slice(pool.id.as_slice());
    push_addr(&mut out, pool.pool);
    push_u8(&mut out, u8::from(pool.coins_indexed_by_uint));
    push_u32(&mut out, pool.coins.len() as u32);
    for coin in &pool.coins {
        push_addr(&mut out, coin.token);
    }
    out
}

pub(super) fn encode_curve_mutable(pool: &CurvePool) -> Vec<u8> {
    let mut out = Vec::new();
    push_word(&mut out, pool.amp);
    push_option_word(&mut out, pool.fee);
    push_word(&mut out, pool.gamma);
    push_word(&mut out, pool.d);
    push_word(&mut out, pool.mid_fee);
    push_word(&mut out, pool.out_fee);
    push_word(&mut out, pool.fee_gamma);
    push_u64(&mut out, pool.flags);
    for coin in &pool.coins {
        push_word(&mut out, coin.balance);
        push_word(&mut out, coin.rate);
        push_word(&mut out, coin.price_scale);
    }
    if pool.coins.iter().any(|coin| !coin.precision.is_zero()) {
        for coin in &pool.coins {
            push_word(&mut out, coin.precision);
        }
    }
    out
}

pub(super) fn decode_curve(immutable: &[u8], body: &[u8]) -> Result<CurvePool, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 4 {
        return Err("curve".into());
    }
    let kind = header.u8()?;
    let id = header.hash()?;
    let pool = header.addr()?;
    let indexed = header.u8()? != 0;
    let n = header.u32()? as usize;
    let mut tokens = Vec::with_capacity(n);
    for _ in 0..n {
        tokens.push(header.addr()?);
    }
    header.finish()?;
    let mut body = Reader::new(body);
    let amp = body.word()?;
    let fee = if body.u8()? == 1 {
        Some(body.word()?)
    } else {
        None
    };
    let gamma = body.word()?;
    let d = body.word()?;
    let mid_fee = body.word()?;
    let out_fee = body.word()?;
    let fee_gamma = body.word()?;
    let flags = body.u64()?;
    let mut coins = Vec::with_capacity(n);
    for token in tokens {
        coins.push(CurveCoin {
            token,
            balance: body.word()?,
            rate: body.word()?,
            price_scale: body.word()?,
            precision: U256::ZERO,
        });
    }
    if body.rest() == n * 32 {
        for coin in &mut coins {
            coin.precision = body.word()?;
        }
    } else if body.rest() != 0 {
        return Err("trailing".into());
    }
    body.finish()?;
    Ok(CurvePool {
        kind,
        id,
        pool,
        amp,
        fee,
        gamma,
        d,
        mid_fee,
        out_fee,
        fee_gamma,
        coins_indexed_by_uint: indexed,
        coins,
        flags,
    })
}

pub(super) fn encode_metric_immutable(pool: &MetricPool) -> Vec<u8> {
    let mut out = vec![5];
    push_u8(&mut out, pool.kind);
    out.extend_from_slice(pool.id.as_slice());
    push_addr(&mut out, pool.pool);
    push_addr(&mut out, pool.token0);
    push_addr(&mut out, pool.token1);
    out
}

pub(super) fn encode_metric_mutable(pool: &MetricPool) -> Vec<u8> {
    let mut out = Vec::new();
    push_u8(&mut out, pool.pause_level);
    push_i32(&mut out, pool.bin_index);
    push_u128(&mut out, pool.position);
    push_i32(&mut out, pool.distance_e6);
    push_u32(&mut out, pool.spread_fee_e6);
    push_u32(&mut out, pool.notional_fee_e8);
    out.extend_from_slice(&pool.asymmetry_x8.to_le_bytes());
    push_word(&mut out, pool.scale0);
    push_word(&mut out, pool.scale1);
    push_i32(&mut out, pool.lowest_bin);
    push_i32(&mut out, pool.highest_bin);
    push_word(&mut out, pool.reference_x64);
    push_addr(&mut out, pool.provider);
    push_u64(&mut out, pool.flags);
    match pool.next_scan {
        Some(value) => {
            push_u8(&mut out, 1);
            push_i32(&mut out, value);
        }
        None => push_u8(&mut out, 0),
    }
    push_u32(&mut out, pool.bins.len() as u32);
    for bin in &pool.bins {
        push_i32(&mut out, bin.index);
        push_u128(&mut out, bin.token0_scaled);
        push_u128(&mut out, bin.token1_scaled);
        out.extend_from_slice(&bin.length_e6.to_le_bytes());
        out.extend_from_slice(&bin.add_fee_buy_e6.to_le_bytes());
        out.extend_from_slice(&bin.add_fee_sell_e6.to_le_bytes());
    }
    out
}

pub(super) fn decode_metric(immutable: &[u8], body: &[u8]) -> Result<MetricPool, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 5 {
        return Err("metric".into());
    }
    let kind = header.u8()?;
    let id = header.hash()?;
    let pool = header.addr()?;
    let token0 = header.addr()?;
    let token1 = header.addr()?;
    header.finish()?;
    let mut body = Reader::new(body);
    let pause_level = body.u8()?;
    let bin_index = body.i32()?;
    let position = body.u128()?;
    let distance_e6 = body.i32()?;
    let spread_fee_e6 = body.u32()?;
    let notional_fee_e8 = body.u32()?;
    let asymmetry_x8 = u16::from_le_bytes(body.take(2)?.try_into().unwrap());
    let scale0 = body.word()?;
    let scale1 = body.word()?;
    let lowest_bin = body.i32()?;
    let highest_bin = body.i32()?;
    let reference_x64 = body.word()?;
    let provider = body.addr()?;
    let flags = body.u64()?;
    let next_scan = if body.u8()? == 1 {
        Some(body.i32()?)
    } else {
        None
    };
    let n = body.u32()? as usize;
    let mut bins = Vec::with_capacity(n);
    for _ in 0..n {
        bins.push(MetricBin {
            index: body.i32()?,
            token0_scaled: body.u128()?,
            token1_scaled: body.u128()?,
            length_e6: u16::from_le_bytes(body.take(2)?.try_into().unwrap()),
            add_fee_buy_e6: u16::from_le_bytes(body.take(2)?.try_into().unwrap()),
            add_fee_sell_e6: u16::from_le_bytes(body.take(2)?.try_into().unwrap()),
        });
    }
    body.finish()?;
    Ok(MetricPool {
        kind,
        id,
        pool,
        pause_level,
        bin_index,
        position,
        distance_e6,
        spread_fee_e6,
        notional_fee_e8,
        asymmetry_x8,
        scale0,
        scale1,
        lowest_bin,
        highest_bin,
        token0,
        token1,
        reference_x64,
        provider,
        next_scan,
        bins,
        flags,
    })
}
