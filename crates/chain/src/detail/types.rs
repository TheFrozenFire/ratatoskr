//! Snapshot of one pool and the query that asks for it.

use crate::seeds::document;
use alloy_primitives::{Address, B256, U256};
use store_model::{VenueRow, BALANCER_V2, BALANCER_V3, UNI_V4};

pub const FLAG_TRUNCATED: u64 = 1;
pub const FLAG_FAILED: u64 = 2;
pub const FLAG_STABLE: u64 = 4;
pub const FLAG_CRYPTO: u64 = 8;
pub const FLAG_NO_REFERENCE: u64 = 16;

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
                nonzero(row.factory, document().by_name("uni-v4").address),
                row.extra.unwrap_or_default(),
            ),
            BALANCER_V2 => (
                nonzero(row.factory, document().by_name("balancer-v2").address),
                row.extra.unwrap_or_default(),
            ),
            BALANCER_V3 => (
                address_of(row.id),
                document().by_name("balancer-v3").address,
            ),
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

pub use store_model::TickWord;

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
    /// Decimal scaling `10^(18-decimals)` from Balancer v3 `getPoolTokenRates`. Empty on v2 and on old rows.
    pub scaling: Vec<U256>,
    /// Token rates beside `scaling`. An empty or zero rate is `1e18` at quote time.
    pub rates: Vec<U256>,
    pub flags: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveCoin {
    pub token: Address,
    pub balance: U256,
    pub rate: U256,
    pub price_scale: U256,
    /// `10^(18-decimals)`. Zero on rows read before the coin precision was stored.
    pub precision: U256,
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
    pub(super) fn flags(&self) -> u64 {
        match self {
            Self::ConstantProduct(pool) => pool.flags,
            Self::Concentrated(pool) => pool.flags,
            Self::Balancer(pool) => pool.flags,
            Self::Curve(pool) => pool.flags,
            Self::Metric(pool) => pool.flags,
            Self::Failed(pool) => pool.flags,
        }
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
