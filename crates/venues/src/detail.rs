//! Pool state for a swap and for the input-output curve.
//!
//! One creation-code `eth_call` of `contracts/lens/DetailLens.sol` reads a
//! batch of pools. Concentrated pools come back with the non-zero tick-bitmap
//! words. A second call reads the liquidity word of each initialized tick:
//! `eth_getStorageValues` when the provider implements it, otherwise one or more
//! `TickLens` calls. Tick reads stay out of the detail call so the two do not
//! share a gas limit or a response-size budget.

use crate::catalog::{
    VenueRow, AERO, BALANCER_V2, BALANCER_V3, CURVE, METRIC, PANCAKE_V2, PANCAKE_V3, SUSHI_V2,
    SUSHI_V3, UNI_V2, UNI_V3, UNI_V4, V2_FORK, V3_FORK,
};
use crate::decode::known;
use crate::lens::CALL_GAS;
use crate::rpc::{Rpc, StorageError};
use crate::tick::{self, PoolTicks};
use alloy_primitives::{keccak256, Address, Uint, B256, I256, U256};
use eyre::eyre;
use std::str::FromStr;
use std::sync::OnceLock;

const MAGIC: [u8; 32] = [
    0x9a, 0x7a, 0xd5, 0x55, 0x5f, 0xe5, 0x50, 0xf2, 0x7c, 0x28, 0x58, 0x1e, 0x7e, 0xf0, 0xa5, 0x54,
    0xb2, 0x1e, 0x5f, 0xbb, 0x38, 0x78, 0x33, 0x3e, 0xe6, 0x1e, 0x72, 0x1f, 0x17, 0x05, 0x53, 0xaa,
];
const CREATION_HEX: &str = include_str!("../../../contracts/lens/DetailLens.bin");
const LENS_BATCH: usize = 16;
const RESUME_ROUNDS: u32 = 8;
const MIN_SQRT: &str = "4295128739";
const MAX_SQRT: &str = "1461446703485210103287273052203988822378723970342";
const MAX_TICK: i32 = 887272;

pub const FLAG_TRUNCATED: u64 = 1;
pub const FLAG_FAILED: u64 = 2;
pub const FLAG_STABLE: u64 = 4;
pub const FLAG_CRYPTO: u64 = 8;
pub const FLAG_NO_REFERENCE: u64 = 16;

type U512 = Uint<512, 8>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanFrom {
    Start,
    At(i32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailQuery {
    pub kind: u8,
    pub id: B256,
    pub home: Address,
    pub aux: Address,
    pub scan_from: ScanFrom,
    pub token0: Address,
    pub token1: Address,
    pub tick_spacing: i32,
}

impl DetailQuery {
    pub fn from_row(row: &VenueRow) -> Self {
        let token0 = row.tokens.first().copied().unwrap_or_default();
        let token1 = row.tokens.get(1).copied().unwrap_or_default();
        let (home, aux) = match row.kind {
            UNI_V4 => (
                nonzero(row.factory, known().pool_manager),
                row.extra.unwrap_or_default(),
            ),
            BALANCER_V2 => (
                nonzero(row.factory, known().bal_v2),
                row.extra.unwrap_or_default(),
            ),
            BALANCER_V3 => (address_of(row.id), known().bal_v3),
            _ => (address_of(row.id), Address::ZERO),
        };
        Self {
            kind: row.kind,
            id: row.id,
            home,
            aux,
            scan_from: ScanFrom::Start,
            token0,
            token1,
            tick_spacing: row.tick_spacing.unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    pub amount_in: U256,
    pub amount_out: U256,
    pub fee_paid: U256,
    /// The pool could not fill the whole input.
    pub exhausted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstantProduct {
    pub kind: u8,
    pub id: B256,
    pub pair: Address,
    pub token0: Address,
    pub token1: Address,
    pub reserve0: U256,
    pub reserve1: U256,
    /// Unset when `fee()` and `swapFee()` both reverted. Kinds 1–4 then use 30 bps,
    /// or 25 bps for Pancake.
    pub fee_pips: Option<u32>,
    pub timestamp: u64,
    pub flags: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitmapWord {
    pub word_pos: i16,
    pub bits: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickWord {
    pub tick: i32,
    pub liquidity_gross: u128,
    pub liquidity_net: i128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Concentrated {
    pub kind: u8,
    pub id: B256,
    /// Pool, or the v4 PoolManager.
    pub home: Address,
    pub hooks: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee_pips: Option<u32>,
    pub protocol_fee: u32,
    pub tick_spacing: i32,
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub liquidity: u128,
    pub next_scan: Option<i32>,
    pub bitmaps: Vec<BitmapWord>,
    pub ticks: Vec<TickWord>,
    pub flags: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalancerPool {
    pub kind: u8,
    pub id: B256,
    pub vault: Address,
    pub pool: Address,
    /// Swap fee with an 1e18 denominator. Unset when the view reverted.
    pub fee: Option<U256>,
    pub amp: U256,
    pub amp_precision: U256,
    pub tokens: Vec<Address>,
    pub balances: Vec<U256>,
    pub weights: Vec<U256>,
    pub flags: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveCoin {
    pub token: Address,
    pub balance: U256,
    pub rate: U256,
    pub price_scale: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurvePool {
    pub kind: u8,
    pub id: B256,
    pub pool: Address,
    /// Amplification `A`, after dividing `A_precise` by 100.
    pub amp: U256,
    pub fee: Option<U256>,
    pub gamma: U256,
    pub d: U256,
    pub mid_fee: U256,
    pub out_fee: U256,
    pub fee_gamma: U256,
    pub coins_indexed_by_uint: bool,
    pub coins: Vec<CurveCoin>,
    pub flags: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricBin {
    pub index: i32,
    pub token0_scaled: u128,
    pub token1_scaled: u128,
    pub length_e6: u16,
    pub add_fee_buy_e6: u16,
    pub add_fee_sell_e6: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricPool {
    pub kind: u8,
    pub id: B256,
    pub pool: Address,
    pub pause_level: u8,
    pub bin_index: i32,
    pub position: u128,
    pub distance_e6: i32,
    pub spread_fee_e6: u32,
    pub notional_fee_e8: u32,
    pub asymmetry_x8: u16,
    pub scale0: U256,
    pub scale1: U256,
    pub lowest_bin: i32,
    pub highest_bin: i32,
    pub token0: Address,
    pub token1: Address,
    pub reference_x64: U256,
    pub provider: Address,
    pub next_scan: Option<i32>,
    pub bins: Vec<MetricBin>,
    pub flags: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedRead {
    pub kind: u8,
    pub id: B256,
    pub home: Address,
    pub flags: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PoolDetail {
    ConstantProduct(ConstantProduct),
    Concentrated(Concentrated),
    Balancer(BalancerPool),
    Curve(CurvePool),
    Metric(MetricPool),
    Failed(FailedRead),
}

impl PoolDetail {
    fn flags(&self) -> u64 {
        match self {
            Self::ConstantProduct(pool) => pool.flags,
            Self::Concentrated(pool) => pool.flags,
            Self::Balancer(pool) => pool.flags,
            Self::Curve(pool) => pool.flags,
            Self::Metric(pool) => pool.flags,
            Self::Failed(pool) => pool.flags,
        }
    }

    pub fn quote(
        &self,
        token_in: Address,
        token_out: Address,
        amount_in: U256,
    ) -> Result<Quote, String> {
        if amount_in.is_zero() {
            return Ok(Quote {
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                fee_paid: U256::ZERO,
                exhausted: false,
            });
        }
        if self.flags() & FLAG_FAILED != 0 {
            return Err("pool read failed".into());
        }
        match self {
            Self::ConstantProduct(pool) => quote_product(pool, token_in, token_out, amount_in),
            Self::Concentrated(pool) => quote_concentrated(pool, token_in, token_out, amount_in),
            Self::Balancer(pool) => quote_balancer(pool, token_in, token_out, amount_in),
            Self::Curve(pool) => quote_curve(pool, token_in, token_out, amount_in),
            Self::Metric(pool) => quote_metric(pool, token_in, token_out, amount_in),
            Self::Failed(_) => Err("pool read failed".into()),
        }
    }
}

pub fn load_details(rpc: &Rpc, rows: &[VenueRow]) -> eyre::Result<Vec<PoolDetail>> {
    let queries = rows.iter().map(DetailQuery::from_row).collect::<Vec<_>>();
    let mut out = Vec::with_capacity(queries.len());
    for chunk in queries.chunks(LENS_BATCH) {
        let mut page = fetch_page(rpc, chunk)?;
        for (query, detail) in chunk.iter().zip(page.iter_mut()) {
            resume_one(rpc, query, detail)?;
        }
        attach_ticks(rpc, &mut page)?;
        out.extend(page);
    }
    Ok(out)
}

fn creation_code() -> &'static [u8] {
    static CODE: OnceLock<Vec<u8>> = OnceLock::new();
    CODE.get_or_init(|| {
        alloy_primitives::hex::decode(CREATION_HEX.trim()).expect("DetailLens.bin is hex")
    })
}

pub fn call_data(queries: &[DetailQuery]) -> Vec<u8> {
    let mut out = creation_code().to_vec();
    push_u256(&mut out, U256::from(0x20));
    push_u256(&mut out, U256::from(queries.len() * 8));
    for query in queries {
        push_u256(&mut out, U256::from(query.kind));
        out.extend_from_slice(query.id.as_slice());
        push_u256(&mut out, address_word(query.home));
        push_u256(&mut out, address_word(query.aux));
        push_u256(&mut out, scan_word(query.scan_from));
        push_u256(&mut out, address_word(query.token0));
        push_u256(&mut out, address_word(query.token1));
        push_u256(&mut out, u256_from_i32(query.tick_spacing));
    }
    out
}

fn fetch_page(rpc: &Rpc, queries: &[DetailQuery]) -> eyre::Result<Vec<PoolDetail>> {
    if queries.is_empty() {
        return Ok(Vec::new());
    }
    let call = rpc.eth_call(&call_data(queries), None, CALL_GAS)?;
    match decode_details(&call.bytes, queries) {
        Ok(rows) => Ok(rows),
        Err(_err) if queries.len() > 1 => {
            let mid = queries.len() / 2;
            let mut rows = fetch_page(rpc, &queries[..mid])?;
            rows.extend(fetch_page(rpc, &queries[mid..])?);
            Ok(rows)
        }
        Err(err) => Err(eyre!("{err}: {}", call.message)),
    }
}

fn resume_one(rpc: &Rpc, query: &DetailQuery, detail: &mut PoolDetail) -> eyre::Result<()> {
    for _ in 0..RESUME_ROUNDS {
        let Some(at) = resume_at(detail) else {
            return Ok(());
        };
        let mut next = query.clone();
        next.scan_from = ScanFrom::At(at);
        let mut page = fetch_page(rpc, &[next])?;
        let Some(page) = page.pop() else {
            return Ok(());
        };
        if resume_at(&page) == Some(at) {
            return Ok(());
        }
        *detail = merge_page(detail.clone(), page);
    }
    Ok(())
}

fn resume_at(detail: &PoolDetail) -> Option<i32> {
    match detail {
        PoolDetail::Concentrated(pool) if pool.flags & FLAG_TRUNCATED != 0 => pool.next_scan,
        PoolDetail::Metric(pool) if pool.flags & FLAG_TRUNCATED != 0 => pool.next_scan,
        _ => None,
    }
}

fn merge_page(base: PoolDetail, page: PoolDetail) -> PoolDetail {
    match (base, page) {
        (PoolDetail::Concentrated(mut base), PoolDetail::Concentrated(page)) => {
            base.bitmaps.extend(page.bitmaps);
            base.next_scan = page.next_scan;
            base.flags = (base.flags & !FLAG_TRUNCATED) | (page.flags & FLAG_TRUNCATED);
            base.flags |= page.flags & !FLAG_TRUNCATED;
            PoolDetail::Concentrated(base)
        }
        (PoolDetail::Metric(mut base), PoolDetail::Metric(page)) => {
            base.bins.extend(page.bins);
            base.next_scan = page.next_scan;
            base.flags = (base.flags & !FLAG_TRUNCATED) | (page.flags & FLAG_TRUNCATED);
            base.flags |= page.flags & !FLAG_TRUNCATED;
            PoolDetail::Metric(base)
        }
        (base, _) => base,
    }
}

fn attach_ticks(rpc: &Rpc, details: &mut [PoolDetail]) -> eyre::Result<()> {
    let mut indexes = Vec::new();
    let mut requests = Vec::new();
    let mut reads = Vec::new();
    for (index, detail) in details.iter().enumerate() {
        let PoolDetail::Concentrated(pool) = detail else {
            continue;
        };
        let ticks = pool.initialized_ticks();
        if ticks.is_empty() {
            continue;
        }
        indexes.push(index);
        let slot_base = tick_slot_base(pool);
        for tick in &ticks {
            let (account, slot) = tick_storage(pool, *tick);
            debug_assert_eq!(account, pool.home);
            debug_assert_eq!(slot, mapping_slot(i256_from_i32(*tick), slot_base));
            reads.push((account, slot));
        }
        requests.push(PoolTicks {
            kind: pool.kind,
            id: pool.id,
            home: pool.home,
            slot_base,
            ticks,
        });
    }
    if requests.is_empty() {
        return Ok(());
    }
    match rpc.storage_words(&reads) {
        Ok(words) => {
            apply_storage_words(details, &indexes, &requests, &words)?;
            return Ok(());
        }
        Err(StorageError::Unsupported) => {}
        Err(StorageError::Failed(err)) => return Err(err),
    }
    let loaded = tick::load_ticks(rpc, &requests)?;
    for ((index, request), (words, failed)) in indexes
        .into_iter()
        .zip(&requests)
        .zip(loaded.ticks.into_iter().zip(loaded.failed))
    {
        if words.len() != request.ticks.len() {
            return Err(eyre!("tick lens dropped words"));
        }
        let mut ticks = words
            .into_iter()
            .map(|read| {
                let (liquidity_gross, liquidity_net) = unpack_tick(read.word);
                TickWord {
                    tick: read.tick,
                    liquidity_gross,
                    liquidity_net,
                }
            })
            .collect::<Vec<_>>();
        ticks.sort_by_key(|tick| tick.tick);
        if let PoolDetail::Concentrated(pool) = &mut details[index] {
            if failed {
                pool.flags |= FLAG_FAILED;
            }
            pool.ticks = ticks;
        }
    }
    Ok(())
}

fn apply_storage_words(
    details: &mut [PoolDetail],
    indexes: &[usize],
    requests: &[PoolTicks],
    words: &[U256],
) -> eyre::Result<()> {
    let mut cursor = 0;
    for (index, request) in indexes.iter().zip(requests) {
        let mut ticks = Vec::with_capacity(request.ticks.len());
        for tick in &request.ticks {
            let word = words
                .get(cursor)
                .copied()
                .ok_or_else(|| eyre!("storage values short"))?;
            cursor += 1;
            let (liquidity_gross, liquidity_net) = unpack_tick(word);
            ticks.push(TickWord {
                tick: *tick,
                liquidity_gross,
                liquidity_net,
            });
        }
        ticks.sort_by_key(|tick| tick.tick);
        if let PoolDetail::Concentrated(pool) = &mut details[*index] {
            pool.ticks = ticks;
        }
    }
    if cursor != words.len() {
        return Err(eyre!("storage values long"));
    }
    Ok(())
}

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
    if n > 8 || weights > 8 || body.len() != 5 + n * 2 + weights {
        return Err("balancer body".into());
    }
    let mut tokens = Vec::with_capacity(n);
    let mut balances = Vec::with_capacity(n);
    for index in 0..n {
        tokens.push(address_of_word(body[5 + index * 2]));
        balances.push(body[6 + index * 2]);
    }
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
        weights: body[5 + n * 2..].to_vec(),
        flags,
    })
}

fn curve(kind: u8, id: B256, flags: u64, body: &[U256]) -> Result<CurvePool, String> {
    if body.len() < 9 {
        return Err("curve body".into());
    }
    let n = fit_u64(body[0]).ok_or_else(|| "curve n".to_string())? as usize;
    if n > 8 || body.len() != 9 + n * 4 {
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

impl Concentrated {
    pub fn initialized_ticks(&self) -> Vec<i32> {
        let mut ticks = Vec::new();
        for map in &self.bitmaps {
            for bit in 0..256 {
                if !map.bits.bit(bit) {
                    continue;
                }
                let compressed = (i32::from(map.word_pos) << 8) + bit as i32;
                let Some(tick) = compressed.checked_mul(self.tick_spacing) else {
                    continue;
                };
                ticks.push(tick);
            }
        }
        ticks.sort_unstable();
        ticks.dedup();
        ticks
    }
}

fn tick_slot_base(pool: &Concentrated) -> U256 {
    match pool.kind {
        PANCAKE_V3 => U256::from(6u8),
        AERO => U256::from(14u8),
        UNI_V4 => v4_state_slot(pool.id) + U256::from(4u8),
        _ => U256::from(5u8),
    }
}

fn tick_storage(pool: &Concentrated, tick: i32) -> (Address, U256) {
    (
        pool.home,
        mapping_slot(i256_from_i32(tick), tick_slot_base(pool)),
    )
}

fn v4_state_slot(id: B256) -> U256 {
    keccak_pair(id, U256::from(6u8))
}

fn mapping_slot(key: I256, slot: U256) -> U256 {
    keccak_pair(B256::from(key.to_be_bytes::<32>()), slot)
}

fn keccak_pair(left: B256, right: U256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(left.as_slice());
    buf[32..].copy_from_slice(&right.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

fn unpack_tick(word: U256) -> (u128, i128) {
    let gross = (word & U256::from(u128::MAX)).to::<u128>();
    let high: U256 = word >> 128;
    let net = high.to::<u128>() as i128;
    (gross, net)
}

fn quote_product(
    pool: &ConstantProduct,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    let (reserve_in, reserve_out) = if token_in == pool.token0 && token_out == pool.token1 {
        (pool.reserve0, pool.reserve1)
    } else if token_in == pool.token1 && token_out == pool.token0 {
        (pool.reserve1, pool.reserve0)
    } else {
        return Err("token".into());
    };
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err("reserves".into());
    }
    let fee = product_fee_pips(pool.kind, pool.fee_pips);
    constant_product_out(
        reserve_in,
        reserve_out,
        amount_in,
        U256::from(fee),
        U256::from(1_000_000),
    )
}

fn product_fee_pips(kind: u8, raw: Option<u32>) -> u32 {
    match raw {
        Some(fee) if fee <= 100 => fee.saturating_mul(100),
        Some(fee) if fee <= 1_000_000 => fee,
        _ => {
            if kind == PANCAKE_V2 {
                2500
            } else {
                3000
            }
        }
    }
}

fn constant_product_out(
    reserve_in: U256,
    reserve_out: U256,
    amount_in: U256,
    fee: U256,
    fee_denom: U256,
) -> Result<Quote, String> {
    if fee > fee_denom {
        return Err("fee".into());
    }
    let after = mul_div(amount_in, fee_denom - fee, fee_denom)?;
    let out = mul_div(reserve_out, after, reserve_in + after)?;
    Ok(Quote {
        amount_in,
        amount_out: out,
        fee_paid: amount_in - after,
        exhausted: false,
    })
}

fn quote_concentrated(
    pool: &Concentrated,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    let zero_for_one = if token_in == pool.token0 && token_out == pool.token1 {
        true
    } else if token_in == pool.token1 && token_out == pool.token0 {
        false
    } else {
        return Err("token".into());
    };
    let fee = pool.fee_pips.ok_or_else(|| "fee unread".to_string())?;
    if fee > 1_000_000 {
        return Err("fee".into());
    }
    if pool.sqrt_price_x96.is_zero() {
        return Err("sqrt price".into());
    }
    let mut sqrt = pool.sqrt_price_x96;
    let mut liquidity = pool.liquidity;
    let mut tick = pool.tick;
    let mut remaining = amount_in;
    let mut amount_out = U256::ZERO;
    let mut fee_paid = U256::ZERO;
    let steps = pool.ticks.len() + 1;
    for _ in 0..=steps {
        if remaining.is_zero() {
            break;
        }
        let boundary = next_initialized(pool, tick, zero_for_one);
        let target = match boundary {
            Some(next) => sqrt_at_tick(next)?,
            None if zero_for_one => min_sqrt(),
            None => max_sqrt(),
        };
        if zero_for_one && target >= sqrt || !zero_for_one && target <= sqrt {
            break;
        }
        if liquidity == 0 {
            let Some(crossed) = boundary else {
                break;
            };
            liquidity = add_liquidity(liquidity, net_on_cross(pool, crossed, zero_for_one))?;
            sqrt = target;
            tick = crossed;
            continue;
        }
        let step = swap_step(sqrt, target, liquidity, remaining, fee, zero_for_one)?;
        remaining -= step.amount_in + step.fee;
        amount_out += step.amount_out;
        fee_paid += step.fee;
        sqrt = step.sqrt_next;
        let Some(crossed) = boundary else {
            break;
        };
        if !step.reached {
            break;
        }
        liquidity = add_liquidity(liquidity, net_on_cross(pool, crossed, zero_for_one))?;
        tick = crossed;
    }
    Ok(Quote {
        amount_in: amount_in - remaining,
        amount_out,
        fee_paid,
        exhausted: !remaining.is_zero(),
    })
}

struct Step {
    amount_in: U256,
    fee: U256,
    amount_out: U256,
    sqrt_next: U256,
    reached: bool,
}

fn swap_step(
    sqrt: U256,
    target: U256,
    liquidity: u128,
    remaining: U256,
    fee_pips: u32,
    zero_for_one: bool,
) -> Result<Step, String> {
    let fee = U256::from(fee_pips);
    let denom = U256::from(1_000_000u64);
    let less_fee = mul_div(remaining, denom - fee, denom)?;
    let (in_to_target, out_to_target) = if zero_for_one {
        (
            amount0_delta(target, sqrt, liquidity, true)?,
            amount1_delta(target, sqrt, liquidity, false)?,
        )
    } else {
        (
            amount1_delta(sqrt, target, liquidity, true)?,
            amount0_delta(sqrt, target, liquidity, false)?,
        )
    };
    if less_fee >= in_to_target {
        let step_fee = if fee.is_zero() {
            U256::ZERO
        } else {
            mul_div_up(in_to_target, fee, denom - fee)?
        };
        if in_to_target + step_fee <= remaining {
            return Ok(Step {
                amount_in: in_to_target,
                fee: step_fee,
                amount_out: out_to_target,
                sqrt_next: target,
                reached: true,
            });
        }
    }
    let sqrt_next = if zero_for_one {
        next_sqrt_from_amount0(sqrt, liquidity, less_fee)?
    } else {
        next_sqrt_from_amount1(sqrt, liquidity, less_fee)?
    };
    let amount_out = if zero_for_one {
        amount1_delta(sqrt_next, sqrt, liquidity, false)?
    } else {
        amount0_delta(sqrt, sqrt_next, liquidity, false)?
    };
    Ok(Step {
        amount_in: less_fee,
        fee: remaining - less_fee,
        amount_out,
        sqrt_next,
        reached: false,
    })
}

fn next_initialized(pool: &Concentrated, tick: i32, zero_for_one: bool) -> Option<i32> {
    if zero_for_one {
        pool.ticks
            .iter()
            .filter(|word| word.tick < tick)
            .map(|word| word.tick)
            .max()
    } else {
        pool.ticks
            .iter()
            .filter(|word| word.tick > tick)
            .map(|word| word.tick)
            .min()
    }
}

fn net_on_cross(pool: &Concentrated, tick: i32, zero_for_one: bool) -> i128 {
    let net = pool
        .ticks
        .iter()
        .find(|word| word.tick == tick)
        .map(|word| word.liquidity_net)
        .unwrap_or(0);
    if zero_for_one {
        net.saturating_neg()
    } else {
        net
    }
}

fn add_liquidity(liquidity: u128, delta: i128) -> Result<u128, String> {
    if delta < 0 {
        liquidity
            .checked_sub(delta.unsigned_abs())
            .ok_or_else(|| "liquidity".to_string())
    } else {
        liquidity
            .checked_add(u128::try_from(delta).map_err(|_| "liquidity")?)
            .ok_or_else(|| "liquidity".to_string())
    }
}

fn amount0_delta(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, String> {
    let (lo, hi) = ordered(sqrt_a, sqrt_b);
    if lo.is_zero() {
        return Err("sqrt".into());
    }
    let numerator1 = U256::from(liquidity) << 96;
    let numerator2 = hi - lo;
    if round_up {
        div_up(mul_div_up(numerator1, numerator2, hi)?, lo)
    } else {
        Ok(mul_div(numerator1, numerator2, hi)? / lo)
    }
}

fn amount1_delta(
    sqrt_a: U256,
    sqrt_b: U256,
    liquidity: u128,
    round_up: bool,
) -> Result<U256, String> {
    let (lo, hi) = ordered(sqrt_a, sqrt_b);
    let diff = hi - lo;
    if round_up {
        mul_div_up(U256::from(liquidity), diff, q96())
    } else {
        mul_div(U256::from(liquidity), diff, q96())
    }
}

fn next_sqrt_from_amount0(sqrt: U256, liquidity: u128, amount: U256) -> Result<U256, String> {
    if amount.is_zero() {
        return Ok(sqrt);
    }
    let numerator1 = U256::from(liquidity) << 96;
    let denom = u512(numerator1)
        .checked_add(widening(amount, sqrt))
        .ok_or_else(|| "sqrt".to_string())?;
    mul_div_up_512(numerator1, sqrt, denom)
}

fn next_sqrt_from_amount1(sqrt: U256, liquidity: u128, amount: U256) -> Result<U256, String> {
    if amount.is_zero() {
        return Ok(sqrt);
    }
    if liquidity == 0 {
        return Err("liquidity".into());
    }
    let quotient = mul_div(amount, q96(), U256::from(liquidity))?;
    sqrt.checked_add(quotient).ok_or_else(|| "sqrt".to_string())
}

fn sqrt_at_tick(tick: i32) -> Result<U256, String> {
    let abs = tick.unsigned_abs();
    if abs > MAX_TICK as u32 {
        return Err("tick".into());
    }
    let mut ratio = if abs & 1 == 0 {
        U256::from(1u8) << 128
    } else {
        u256_hex("fffcb933bd6fad37aa2d162d1a594001")
    };
    for bit in 1..20 {
        if abs & (1 << bit) != 0 {
            ratio = mul_shift_128(ratio, tick_factor(bit));
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }
    let rounded = (ratio >> 32)
        + if (ratio & U256::from(u32::MAX)).is_zero() {
            U256::ZERO
        } else {
            U256::from(1u8)
        };
    Ok(rounded)
}

fn tick_factor(bit: u32) -> U256 {
    let hex = match bit {
        1 => "fff97272373d413259a46990580e213a",
        2 => "fff2e50f5f656932ef12357cf3c7fdcc",
        3 => "ffe5caca7e10e4e61c3624eaa0941cd0",
        4 => "ffcb9843d60f6159c9db58835c926644",
        5 => "ff973b41fa98c081472e6896dfb254c0",
        6 => "ff2ea16466c96a3843ec78b326b52861",
        7 => "fe5dee046a99a2a811c461f1969c3053",
        8 => "fcbe86c7900a88aedcffc83b479aa3a4",
        9 => "f987a7253ac413176f2b074cf7815e54",
        10 => "f3392b0822b70005940c7a398e4b70f3",
        11 => "e7159475a2c29b7443b29c7fa6e889d9",
        12 => "d097f3bdfd2022b8845ad8f792aa5825",
        13 => "a9f746462d870fdf8a65dc1f90e061e5",
        14 => "70d869a156d2a1b890bb3df62baf32f7",
        15 => "31be135f97d08fd981231505542fcfa6",
        16 => "9aa508b5b7a84e1c677de54f3e99bc9",
        17 => "5d6af8dedb81196699c329225ee604",
        18 => "2216e584f5fa1ea926041bedfe98",
        _ => "48a170391f7dc42444e8fa2",
    };
    u256_hex(hex)
}

fn quote_balancer(
    pool: &BalancerPool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    let input = pool
        .tokens
        .iter()
        .position(|token| *token == token_in)
        .ok_or("token")?;
    let output = pool
        .tokens
        .iter()
        .position(|token| *token == token_out)
        .ok_or("token")?;
    if input == output || pool.balances.len() != pool.tokens.len() {
        return Err("token".into());
    }
    let fee = pool.fee.ok_or_else(|| "fee unread".to_string())?;
    let one = e18();
    if pool.flags & FLAG_STABLE != 0 {
        if pool.amp_precision.is_zero() {
            return Err("amp".into());
        }
        let amp = pool.amp / pool.amp_precision;
        return stable_out(&pool.balances, amp, input, output, amount_in, fee, one);
    }
    if pool.weights.len() != pool.tokens.len() {
        return Err("weights".into());
    }
    if pool.weights[input] == pool.weights[output] && !pool.weights[input].is_zero() {
        return constant_product_out(
            pool.balances[input],
            pool.balances[output],
            amount_in,
            fee,
            one,
        );
    }
    weighted_out(
        pool.balances[input],
        pool.balances[output],
        pool.weights[input],
        pool.weights[output],
        amount_in,
        fee,
    )
}

fn weighted_out(
    balance_in: U256,
    balance_out: U256,
    weight_in: U256,
    weight_out: U256,
    amount_in: U256,
    fee: U256,
) -> Result<Quote, String> {
    let one = e18();
    if fee > one || weight_in.is_zero() || weight_out.is_zero() {
        return Err("weights".into());
    }
    let after = mul_div(amount_in, one - fee, one)?;
    let base = f64_of(balance_in)? / f64_of(balance_in + after)?;
    let exponent = f64_of(weight_in)? / f64_of(weight_out)?;
    let kept = base.powf(exponent);
    let out = f64_of(balance_out)? * (1.0 - kept);
    Ok(Quote {
        amount_in,
        amount_out: f64_floor(out)?,
        fee_paid: amount_in - after,
        exhausted: false,
    })
}

fn quote_curve(
    pool: &CurvePool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    let input = pool
        .coins
        .iter()
        .position(|coin| coin.token == token_in)
        .ok_or("token")?;
    let output = pool
        .coins
        .iter()
        .position(|coin| coin.token == token_out)
        .ok_or("token")?;
    if input == output {
        return Err("token".into());
    }
    if pool.flags & FLAG_CRYPTO != 0 || !pool.gamma.is_zero() {
        return quote_crypto(pool, input, output, amount_in);
    }
    let fee = pool.fee.ok_or_else(|| "fee unread".to_string())?;
    let balances = pool
        .coins
        .iter()
        .map(xp_balance)
        .collect::<Result<Vec<_>, _>>()?;
    let dx = xp_amount(&pool.coins[input], amount_in)?;
    stable_out(
        &balances,
        pool.amp,
        input,
        output,
        dx,
        fee,
        U256::from(10_000_000_000u64),
    )
    .and_then(|quote| {
        let amount_out = raw_amount(&pool.coins[output], quote.amount_out)?;
        Ok(Quote {
            amount_in,
            amount_out,
            fee_paid: quote.fee_paid,
            exhausted: quote.exhausted,
        })
    })
}

fn quote_crypto(
    pool: &CurvePool,
    input: usize,
    output: usize,
    amount_in: U256,
) -> Result<Quote, String> {
    let fee = if pool.mid_fee.is_zero() {
        pool.fee.ok_or_else(|| "fee unread".to_string())?
    } else {
        pool.mid_fee
    };
    let denom = U256::from(10_000_000_000u64);
    if fee > denom {
        return Err("fee".into());
    }
    let after = mul_div(amount_in, denom - fee, denom)?;
    let scale = pool.coins[output].price_scale;
    let amount_out = if input == 0 && !scale.is_zero() {
        mul_div(after, e18(), scale)?
    } else if output == 0 && !pool.coins[input].price_scale.is_zero() {
        mul_div(after, pool.coins[input].price_scale, e18())?
    } else {
        return Err("price scale".into());
    };
    Ok(Quote {
        amount_in,
        amount_out,
        fee_paid: amount_in - after,
        exhausted: false,
    })
}

fn stable_out(
    balances: &[U256],
    amp: U256,
    input: usize,
    output: usize,
    amount_in: U256,
    fee: U256,
    fee_denom: U256,
) -> Result<Quote, String> {
    if amp.is_zero() || fee > fee_denom {
        return Err("amp".into());
    }
    let d = get_d(balances, amp)?;
    let x = balances[input]
        .checked_add(amount_in)
        .ok_or_else(|| "amount".to_string())?;
    let y = get_y(input, output, x, balances, amp, d)?;
    let mut dy = balances[output]
        .checked_sub(y)
        .ok_or_else(|| "y".to_string())?
        .checked_sub(U256::from(1u8))
        .ok_or_else(|| "y".to_string())?;
    let uncharged = dy;
    if !fee.is_zero() {
        dy = mul_div(dy, fee_denom - fee, fee_denom)?;
    }
    Ok(Quote {
        amount_in,
        amount_out: dy,
        fee_paid: uncharged - dy,
        exhausted: false,
    })
}

fn get_d(xp: &[U256], amp: U256) -> Result<U256, String> {
    let n = U256::from(xp.len());
    let sum = xp
        .iter()
        .try_fold(U256::ZERO, |acc, value| acc.checked_add(*value))
        .ok_or("sum")?;
    if sum.is_zero() {
        return Ok(U256::ZERO);
    }
    let ann = amp.checked_mul(n).ok_or("amp")?;
    let mut d = sum;
    for _ in 0..255 {
        let mut d_p = d;
        for value in xp {
            if value.is_zero() {
                return Err("empty coin".into());
            }
            d_p = mul_div(d_p, d, value.checked_mul(n).ok_or("n")?)?;
        }
        let prev = d;
        let numerator = ann
            .checked_mul(sum)
            .ok_or("ann")?
            .checked_add(d_p.checked_mul(n).ok_or("dp")?)
            .ok_or("num")?;
        let denominator = (ann - U256::from(1u8))
            .checked_mul(d)
            .ok_or("den")?
            .checked_add((n + U256::from(1u8)).checked_mul(d_p).ok_or("den")?)
            .ok_or("den")?;
        d = mul_div(numerator, d, denominator)?;
        let diff = if d > prev { d - prev } else { prev - d };
        if diff <= U256::from(1u8) {
            return Ok(d);
        }
    }
    Err("invariant".into())
}

fn get_y(i: usize, j: usize, x: U256, xp: &[U256], amp: U256, d: U256) -> Result<U256, String> {
    let n = U256::from(xp.len());
    let ann = amp.checked_mul(n).ok_or("amp")?;
    let mut c = d;
    let mut s = U256::ZERO;
    for (k, balance) in xp.iter().enumerate() {
        if k == j {
            continue;
        }
        let x_k = if k == i { x } else { *balance };
        s = s.checked_add(x_k).ok_or("s")?;
        c = mul_div(c, d, x_k.checked_mul(n).ok_or("xn")?)?;
    }
    c = mul_div(c, d, ann.checked_mul(n).ok_or("ann")?)?;
    let b = s.checked_add(d / ann).ok_or("b")?;
    let mut y = d;
    for _ in 0..255 {
        let prev = y;
        let numerator = y.checked_mul(y).ok_or("y")?.checked_add(c).ok_or("y")?;
        let denominator = (y * U256::from(2u8))
            .checked_add(b)
            .ok_or("y")?
            .checked_sub(d)
            .ok_or("y")?;
        y = numerator / denominator;
        let diff = if y > prev { y - prev } else { prev - y };
        if diff <= U256::from(1u8) {
            return Ok(y);
        }
    }
    Err("y".into())
}

fn xp_balance(coin: &CurveCoin) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(coin.balance)
    } else {
        mul_div(coin.balance, coin.rate, e18())
    }
}

fn xp_amount(coin: &CurveCoin, amount: U256) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(amount)
    } else {
        mul_div(amount, coin.rate, e18())
    }
}

fn raw_amount(coin: &CurveCoin, amount_xp: U256) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(amount_xp)
    } else {
        mul_div(amount_xp, e18(), coin.rate)
    }
}

fn quote_metric(
    pool: &MetricPool,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    if pool.flags & FLAG_NO_REFERENCE != 0 || pool.reference_x64.is_zero() {
        return Err("reference price".into());
    }
    if pool.token0.is_zero() || pool.token1.is_zero() {
        return Err("token".into());
    }
    let price = pool.marginal_price_x64()?;
    let bin = pool.bins.iter().find(|bin| bin.index == pool.bin_index);
    let selling_token0 = if token_in == pool.token0 && token_out == pool.token1 {
        true
    } else if token_in == pool.token1 && token_out == pool.token0 {
        false
    } else {
        return Err("token".into());
    };
    let (scale_out, scaled_out) = if selling_token0 {
        (pool.scale1, bin.map(|bin| bin.token1_scaled).unwrap_or(0))
    } else {
        (pool.scale0, bin.map(|bin| bin.token0_scaled).unwrap_or(0))
    };
    let amount_out = if selling_token0 {
        mul_div(amount_in, price, U256::from(1u8) << 64)?
    } else if price.is_zero() {
        return Err("price".into());
    } else {
        mul_div(amount_in, U256::from(1u8) << 64, price)?
    };
    let raw_available = if scale_out.is_zero() || scaled_out == 0 {
        None
    } else {
        Some(U256::from(scaled_out) / scale_out)
    };
    if let Some(available) = raw_available {
        if amount_out > available {
            return Ok(Quote {
                amount_in,
                amount_out: available,
                fee_paid: U256::ZERO,
                exhausted: true,
            });
        }
    }
    Ok(Quote {
        amount_in,
        amount_out,
        fee_paid: U256::ZERO,
        exhausted: false,
    })
}

impl MetricPool {
    pub fn marginal_price_x64(&self) -> Result<U256, String> {
        if self.reference_x64.is_zero() {
            return Err("reference price".into());
        }
        let length = self
            .bins
            .iter()
            .find(|bin| bin.index == self.bin_index)
            .map(|bin| U256::from(bin.length_e6))
            .unwrap_or(U256::ZERO);
        let lower = distance_price(self.reference_x64, self.distance_e6)?;
        let upper = distance_price(
            self.reference_x64,
            self.distance_e6.saturating_add(length.to::<i32>()),
        )?;
        if upper < lower {
            return Err("bin price".into());
        }
        let span = upper - lower;
        let max_pos = (U256::from(1u8) << 104) - U256::from(1u8);
        let pos = U256::from(self.position);
        if span.is_zero() || pos.is_zero() {
            return Ok(lower);
        }
        Ok(lower + mul_div(span, pos, max_pos)?)
    }
}

fn distance_price(reference: U256, distance_e6: i32) -> Result<U256, String> {
    let million = U256::from(1_000_000u64);
    if distance_e6 >= 0 {
        mul_div(reference, million + U256::from(distance_e6 as u32), million)
    } else {
        let distance = distance_e6.unsigned_abs();
        if U256::from(distance) >= million {
            return Err("distance".into());
        }
        mul_div(reference, million - U256::from(distance), million)
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

fn q96() -> U256 {
    U256::from(1u8) << 96
}

fn mul_div(a: U256, b: U256, denom: U256) -> Result<U256, String> {
    if denom.is_zero() {
        return Err("div0".into());
    }
    let (q, _) = widening(a, b).div_rem(u512(denom));
    narrow(q).ok_or_else(|| "overflow".to_string())
}

fn mul_div_up(a: U256, b: U256, denom: U256) -> Result<U256, String> {
    mul_div_up_512(a, b, u512(denom))
}

fn mul_div_up_512(a: U256, b: U256, denom: U512) -> Result<U256, String> {
    if denom.is_zero() {
        return Err("div0".into());
    }
    let (mut q, r) = widening(a, b).div_rem(denom);
    if !r.is_zero() {
        q += U512::from(1u8);
    }
    narrow(q).ok_or_else(|| "overflow".to_string())
}

fn div_up(value: U256, denom: U256) -> Result<U256, String> {
    if denom.is_zero() {
        return Err("div0".into());
    }
    let (q, r) = value.div_rem(denom);
    if r.is_zero() {
        Ok(q)
    } else {
        q.checked_add(U256::from(1u8))
            .ok_or_else(|| "overflow".to_string())
    }
}

fn widening(a: U256, b: U256) -> U512 {
    a.widening_mul(b)
}

fn mul_shift_128(a: U256, b: U256) -> U256 {
    narrow(widening(a, b) >> 128).expect("tick factor")
}

fn u512(value: U256) -> U512 {
    let limbs = value.as_limbs();
    U512::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3], 0, 0, 0, 0])
}

fn narrow(value: U512) -> Option<U256> {
    let limbs = value.as_limbs();
    if limbs[4] != 0 || limbs[5] != 0 || limbs[6] != 0 || limbs[7] != 0 {
        return None;
    }
    Some(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

fn e18() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}

fn f64_of(value: U256) -> Result<f64, String> {
    value
        .to_string()
        .parse::<f64>()
        .map_err(|_| "scale".to_string())
}

fn f64_floor(value: f64) -> Result<U256, String> {
    if !value.is_finite() || value < 0.0 {
        return Err("quote".into());
    }
    U256::from_str(&format!("{value:.0}")).map_err(|_| "quote".to_string())
}

fn min_sqrt() -> U256 {
    U256::from_str(MIN_SQRT).expect("min sqrt")
}

fn max_sqrt() -> U256 {
    U256::from_str(MAX_SQRT).expect("max sqrt")
}

fn u256_hex(text: &str) -> U256 {
    U256::from_str_radix(text, 16).expect("hex")
}

fn ordered(left: U256, right: U256) -> (U256, U256) {
    if left < right {
        (left, right)
    } else {
        (right, left)
    }
}

fn nonzero(value: Address, fallback: Address) -> Address {
    if value.is_zero() {
        fallback
    } else {
        value
    }
}

fn address_of(id: B256) -> Address {
    Address::from_slice(&id.as_slice()[12..])
}

fn address_of_word(word: U256) -> Address {
    Address::from_slice(&word.to_be_bytes::<32>()[12..])
}

fn address_word(address: Address) -> U256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    U256::from_be_bytes(bytes)
}

fn scan_word(scan: ScanFrom) -> U256 {
    match scan {
        ScanFrom::Start => U256::from_be_bytes(scan_all()),
        ScanFrom::At(value) => u256_from_i32(value),
    }
}

fn scan_all() -> [u8; 32] {
    let mut bytes = [0xff; 32];
    bytes[0] = 0x7f;
    bytes
}

fn u256_from_i32(value: i32) -> U256 {
    U256::from_be_bytes::<32>(i256_from_i32(value).to_be_bytes::<32>())
}

fn i256_from_i32(value: i32) -> I256 {
    let mut bytes = if value < 0 { [0xff; 32] } else { [0; 32] };
    bytes[28..].copy_from_slice(&value.to_be_bytes());
    I256::from_be_bytes(bytes)
}

fn push_u256(out: &mut Vec<u8>, word: U256) {
    out.extend_from_slice(&word.to_be_bytes::<32>());
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, keccak256};

    fn query(kind: u8) -> DetailQuery {
        DetailQuery {
            kind,
            id: B256::repeat_byte(0x11),
            home: address!("1111111111111111111111111111111111111111"),
            aux: Address::ZERO,
            scan_from: ScanFrom::Start,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            tick_spacing: 60,
        }
    }

    fn push(buf: &mut Vec<u8>, word: U256) {
        buf.extend_from_slice(&word.to_be_bytes::<32>());
    }

    #[test]
    fn bytecode_carries_the_detail_magic() {
        assert_eq!(keccak256("ratatoskr.detail.v1").0, MAGIC);
        let code = creation_code();
        assert!(code.len() < 49_152);
        assert!(code
            .windows(33)
            .any(|window| window[0] == 0x7f && window[1..] == MAGIC));
    }

    #[test]
    fn constructor_argument_is_a_dynamic_array() {
        let data = call_data(&[query(UNI_V2)]);
        let args = &data[creation_code().len()..];
        assert_eq!(args.len(), 32 * (2 + 8));
        assert_eq!(args[31], 0x20);
        assert_eq!(args[63], 8);
        assert_eq!(args[95], UNI_V2);
    }

    #[test]
    fn constant_product_fee_scales_hundredths_and_defaults() {
        let mut pool = ConstantProduct {
            kind: V2_FORK,
            id: B256::ZERO,
            pair: Address::ZERO,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            reserve0: U256::from(1_000_000u64),
            reserve1: U256::from(1_000_000u64),
            fee_pips: Some(30),
            timestamp: 0,
            flags: 0,
        };
        let quote = pool_quote(&pool, 10_000);
        assert_eq!(quote.fee_paid, U256::from(30u8));
        pool.kind = PANCAKE_V2;
        pool.fee_pips = None;
        let quote = pool_quote(&pool, 10_000);
        assert_eq!(quote.fee_paid, U256::from(25u8));
        pool.kind = UNI_V2;
        pool.fee_pips = None;
        let quote = pool_quote(&pool, 10_000);
        assert_eq!(quote.fee_paid, U256::from(30u8));
        assert_eq!(quote.amount_out, U256::from(9871u64));
    }

    fn pool_quote(pool: &ConstantProduct, amount: u64) -> Quote {
        PoolDetail::ConstantProduct(pool.clone())
            .quote(pool.token0, pool.token1, U256::from(amount))
            .unwrap()
    }

    #[test]
    fn in_range_concentrated_step_matches_the_fee() {
        let pool = Concentrated {
            kind: UNI_V3,
            id: B256::ZERO,
            home: Address::ZERO,
            hooks: Address::ZERO,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            fee_pips: Some(0),
            protocol_fee: 0,
            tick_spacing: 60,
            sqrt_price_x96: U256::from(1u8) << 96,
            tick: 0,
            liquidity: 1_000_000,
            next_scan: None,
            bitmaps: Vec::new(),
            ticks: Vec::new(),
            flags: 0,
        };
        let quote = PoolDetail::Concentrated(pool.clone())
            .quote(pool.token0, pool.token1, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(999u64));
        assert!(!quote.exhausted);
        let mut charged = pool;
        charged.fee_pips = Some(3000);
        let quote = PoolDetail::Concentrated(charged.clone())
            .quote(charged.token0, charged.token1, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.fee_paid, U256::from(3u8));
        assert_eq!(quote.amount_out, U256::from(996u64));
    }

    #[test]
    fn tick_zero_is_q96_and_the_usdc_weth_slot_matches() {
        assert_eq!(sqrt_at_tick(0).unwrap(), U256::from(1u8) << 96);
        assert!(sqrt_at_tick(1).unwrap() > U256::from(1u8) << 96);
        let pool = Concentrated {
            kind: UNI_V3,
            id: B256::ZERO,
            home: address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"),
            hooks: Address::ZERO,
            token0: Address::ZERO,
            token1: Address::ZERO,
            fee_pips: Some(3000),
            protocol_fee: 0,
            tick_spacing: 60,
            sqrt_price_x96: U256::ZERO,
            tick: 0,
            liquidity: 0,
            next_scan: None,
            bitmaps: vec![BitmapWord {
                word_pos: 12,
                bits: U256::from(1u8),
            }],
            ticks: Vec::new(),
            flags: 0,
        };
        assert_eq!(pool.initialized_ticks(), vec![184_320]);
        let (_, slot) = tick_storage(&pool, 184_320);
        assert_eq!(
            format!("{slot:064x}"),
            "57c9d69987b7546eb9e8011ddacb32024fac00fff6db7eee60be3d96e6c26961"
        );
        let id = B256::repeat_byte(0x11);
        assert_eq!(
            format!("{:064x}", v4_state_slot(id)),
            "260963edbfdc77b27c31140516df0b13cbfaf61421578f81765a4f60f048b151"
        );
    }

    #[test]
    fn product_payload_round_trips() {
        let query = query(UNI_V2);
        let mut payload = Vec::new();
        push(&mut payload, U256::from_be_bytes(MAGIC));
        push(&mut payload, U256::from(1u8));
        push(&mut payload, U256::from(10u8));
        push(&mut payload, U256::from(UNI_V2));
        push(&mut payload, U256::from_be_bytes(query.id.0));
        push(&mut payload, U256::ZERO);
        push(&mut payload, address_word(query.token0));
        push(&mut payload, address_word(query.token1));
        push(&mut payload, U256::from(5u8));
        push(&mut payload, U256::from(7u8));
        push(&mut payload, U256::MAX);
        push(&mut payload, U256::from(9u8));
        let rows = decode_details(&payload, &[query.clone()]).unwrap();
        let PoolDetail::ConstantProduct(pool) = &rows[0] else {
            panic!("product");
        };
        assert_eq!(pool.pair, query.home);
        assert_eq!(pool.reserve0, U256::from(5u8));
        assert_eq!(pool.fee_pips, None);
        assert_eq!(pool.timestamp, 9);
    }

    #[test]
    fn stableswap_two_coin_quote_matches_the_integer_invariant() {
        let token0 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token1 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let coin = |token| CurveCoin {
            token,
            balance: e18(),
            rate: U256::ZERO,
            price_scale: U256::ZERO,
        };
        let pool = CurvePool {
            kind: CURVE,
            id: B256::ZERO,
            pool: Address::ZERO,
            amp: U256::from(100u8),
            fee: Some(U256::ZERO),
            gamma: U256::ZERO,
            d: U256::ZERO,
            mid_fee: U256::ZERO,
            out_fee: U256::ZERO,
            fee_gamma: U256::ZERO,
            coins_indexed_by_uint: true,
            coins: vec![coin(token0), coin(token1)],
            flags: 0,
        };
        let quote = PoolDetail::Curve(pool)
            .quote(token0, token1, U256::from(10_000_000_000_000_000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(9_999_009_901_970_393u64));
    }

    #[test]
    fn equal_weight_balancer_is_constant_product() {
        let token0 = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token1 = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let pool = BalancerPool {
            kind: BALANCER_V2,
            id: B256::ZERO,
            vault: Address::ZERO,
            pool: Address::ZERO,
            fee: Some(U256::ZERO),
            amp: U256::ZERO,
            amp_precision: U256::ZERO,
            tokens: vec![token0, token1],
            balances: vec![U256::from(1_000_000u64), U256::from(1_000_000u64)],
            weights: vec![e18() / U256::from(2u8), e18() / U256::from(2u8)],
            flags: 0,
        };
        let quote = PoolDetail::Balancer(pool)
            .quote(token0, token1, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(999u64));
    }

    #[test]
    fn metric_price_walks_the_bin_and_caps_at_the_balance() {
        let mut pool = sample_metric();
        let token_in = pool.token0;
        let token_out = pool.token1;
        assert_eq!(pool.marginal_price_x64().unwrap(), U256::from(1u8) << 64);
        pool.distance_e6 = 0;
        pool.bins[0].length_e6 = 65_535;
        pool.position = (1u128 << 104) - 1;
        let upper = ((U256::from(1u8) << 64) * U256::from(1_000_000u64 + 65_535))
            / U256::from(1_000_000u64);
        assert_eq!(pool.marginal_price_x64().unwrap(), upper);
        pool.position = 0;
        pool.bins[0].length_e6 = 0;
        let quote = PoolDetail::Metric(pool.clone())
            .quote(token_in, token_out, U256::from(1000u64))
            .unwrap();
        assert_eq!(quote.amount_out, U256::from(1000u64));
        let mut thin = pool;
        thin.bins[0].token1_scaled = 40;
        let quote = PoolDetail::Metric(thin)
            .quote(token_in, token_out, U256::from(1000u64))
            .unwrap();
        assert!(quote.exhausted);
        assert_eq!(quote.amount_out, U256::from(40u8));
    }

    fn sample_metric() -> MetricPool {
        MetricPool {
            kind: METRIC,
            id: B256::ZERO,
            pool: Address::ZERO,
            pause_level: 0,
            bin_index: 0,
            position: 0,
            distance_e6: 0,
            spread_fee_e6: 0,
            notional_fee_e8: 0,
            asymmetry_x8: 0,
            scale0: U256::from(1u8),
            scale1: U256::from(1u8),
            lowest_bin: 0,
            highest_bin: 0,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            reference_x64: U256::from(1u8) << 64,
            provider: Address::ZERO,
            next_scan: None,
            bins: vec![MetricBin {
                index: 0,
                token0_scaled: 1_000,
                token1_scaled: 1_000,
                length_e6: 0,
                add_fee_buy_e6: 0,
                add_fee_sell_e6: 0,
            }],
            flags: 0,
        }
    }

    #[test]
    fn v4_query_uses_the_manager_and_the_hooks() {
        let hooks = address!("5555555555555555555555555555555555555555");
        let row = VenueRow {
            kind: UNI_V4,
            id: B256::repeat_byte(0x22),
            factory: known().pool_manager,
            tokens: vec![
                address!("0000000000000000000000000000000000000000"),
                address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            ],
            fee: Some(3000),
            tick_spacing: Some(60),
            block_number: None,
            extra: Some(hooks),
            block_hash: None,
            tx_index: None,
            log_index: None,
        };
        let query = DetailQuery::from_row(&row);
        assert_eq!(query.home, known().pool_manager);
        assert_eq!(query.aux, hooks);
        assert_eq!(query.tick_spacing, 60);
    }

    #[test]
    #[ignore = "Alchemy detail call plus tick lens for the USDC/WETH 0.3% pool"]
    fn live_uni_v3_usdc_weth_tick_word_matches_ticks() {
        let rpc = Rpc::new(vec![alchemy_url()])
            .unwrap_or_else(|err| panic!("{}", redact(&format!("{err:#}"))));
        let pool = address!("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8");
        let row = VenueRow {
            kind: UNI_V3,
            id: VenueRow::address_id(pool),
            factory: known().uni_v3,
            tokens: vec![
                address!("a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
                address!("c02aaa39b223fe8d0a0e5c4f27ead9083c756cc2"),
            ],
            fee: Some(3000),
            tick_spacing: Some(60),
            block_number: None,
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        };
        let details = load_details(&rpc, &[row])
            .unwrap_or_else(|err| panic!("{}", redact(&format!("{err:#}"))));
        let PoolDetail::Concentrated(detail) = &details[0] else {
            panic!("uniswap v3 detail");
        };
        assert_eq!(detail.fee_pips, Some(3000));
        assert_eq!(detail.tick_spacing, 60);
        assert_eq!(detail.home, pool);
        assert!(!detail.sqrt_price_x96.is_zero());
        assert!(detail.liquidity > 0);
        assert!(!detail.ticks.is_empty(), "bitmap produced no ticks");
        assert_eq!(detail.ticks.len(), detail.initialized_ticks().len());
        assert_eq!(detail.flags & FLAG_FAILED, 0);
        for sample in [
            detail.ticks.first(),
            detail.ticks.get(detail.ticks.len() / 2),
        ]
        .into_iter()
        .flatten()
        {
            assert_tick_view(&rpc, pool, sample);
        }
    }

    fn assert_tick_view(rpc: &Rpc, pool: Address, sample: &TickWord) {
        let mut data = vec![0xf3, 0x0d, 0xba, 0x93];
        data.extend_from_slice(&u256_from_i32(sample.tick).to_be_bytes::<32>());
        let call = rpc
            .eth_call(&data, Some(pool), 1_000_000)
            .unwrap_or_else(|err| panic!("{}", redact(&format!("{err:#}"))));
        assert!(!call.reverted, "{}", redact(&call.message));
        assert!(call.bytes.len() >= 64, "tick {}", sample.tick);
        // `ticks()` ABI-encodes gross and net as separate words. The lens packs them into one.
        let gross = U256::from_be_slice(&call.bytes[..32]);
        let net = I256::from_be_bytes::<32>(call.bytes[32..64].try_into().unwrap());
        assert_eq!(
            gross,
            U256::from(sample.liquidity_gross),
            "tick {}",
            sample.tick
        );
        assert_eq!(
            net,
            I256::try_from(sample.liquidity_net).unwrap(),
            "tick {}",
            sample.tick
        );
    }

    fn alchemy_url() -> String {
        let text = std::fs::read_to_string("/home/jmart/git/TheFrozenFire/ratatoskr/.env")
            .expect("env file");
        for line in text.lines() {
            let Some(key) = line.strip_prefix("ALCHEMY_API_KEY=") else {
                continue;
            };
            let key = key.trim().trim_matches('"');
            assert!(!key.is_empty());
            return format!("https://eth-mainnet.g.alchemy.com/v2/{key}");
        }
        panic!("ALCHEMY_API_KEY");
    }

    fn redact(text: &str) -> String {
        let marker = "eth-mainnet.g.alchemy.com/v2/";
        let Some(start) = text.find(marker) else {
            return text.to_string();
        };
        let from = start + marker.len();
        let end = text[from..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
            .map(|index| from + index)
            .unwrap_or(text.len());
        let mut out = text.to_string();
        out.replace_range(from..end, "redacted");
        out
    }
}
