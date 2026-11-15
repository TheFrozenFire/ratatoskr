//! Local sample of one pool snapshot.
//!
//! A direction's domain is the largest input that still has a price. The grid
//! is fractions of that input. Each sample carries this crate's quote of the
//! ingested snapshot.

use super::domain::{domain_max, grid_amounts};
use super::formula::fit_json;
use crate::state::{is_canonical, join_detail, CurveRecord, PoolState};
use alloy_primitives::{Address, B256, U256};
use chain::detail::{PoolDetail, Quote};
use fitter::decode_fit;
use num_bigint::{BigInt, Sign};
use serde_json::Value;

pub(super) const MAX_LEGS: usize = 4;
pub(super) const BISECT_STEPS: u8 = 8;
pub(super) const PAGE: usize = 32;

pub(super) struct Sample {
    pub(super) role: &'static str,
    pub(super) fraction: Option<&'static str>,
    pub(super) amount_in: U256,
    pub(super) local_out: Option<U256>,
    pub(super) local_fee: Option<U256>,
    pub(super) local_exhausted: Option<bool>,
    pub(super) local_error: Option<String>,
    pub(super) chain_out: Option<U256>,
    pub(super) chain_error: Option<String>,
    pub(super) quoter: Option<Address>,
    pub(super) method: Option<&'static str>,
    pub(super) attempted: bool,
    pub(super) alt: Option<Vec<u8>>,
    pub(super) alt_method: Option<&'static str>,
}

pub(super) struct Direction {
    pub(super) token_in: Address,
    pub(super) token_out: Address,
    pub(super) max_in: U256,
    pub(super) max_source: &'static str,
    pub(super) chain_max_in: Option<U256>,
    pub(super) larger_input_reverted: bool,
    pub(super) samples: Vec<Sample>,
}

pub(super) struct PoolPlan {
    pub(super) kind: u8,
    pub(super) id: B256,
    pub(super) block: u64,
    pub(super) generation: u64,
    pub(super) truncated: bool,
    pub(super) tick_count: u32,
    pub(super) bitmap: B256,
    pub(super) factory: Option<Address>,
    pub(super) identity_fee: Option<u32>,
    pub(super) identity_spacing: Option<i32>,
    pub(super) created_block: Option<u64>,
    pub(super) canonical: Option<bool>,
    pub(super) curve_version: Option<u32>,
    pub(super) skip_reason: Option<u8>,
    pub(super) fits: Vec<Value>,
    pub(super) pairs_capped: bool,
    pub(super) missing: Option<String>,
    pub(super) detail: Option<PoolDetail>,
    pub(super) directions: Vec<Direction>,
}

/// Smallest grid knot is `1/10_000` of `max_in`. A smaller domain rounds to 0.
pub(crate) const SAMPLE_FLOOR: u64 = 10_000;

/// How much of the quote grid this snapshot can fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TradeCapacity {
    pub any_positive: bool,
    pub fills_sample: bool,
    pub top: U256,
}

/// Largest priced input on the snapshot, without building quote samples.
pub(crate) fn trade_capacity(detail: &PoolDetail) -> TradeCapacity {
    let mut out = TradeCapacity {
        any_positive: false,
        fills_sample: false,
        top: U256::ZERO,
    };
    let floor = U256::from(SAMPLE_FLOOR);
    let (_, pairs) = pairs_of(detail);
    for (token_in, token_out) in pairs {
        let (max_in, _) = domain_max(detail, token_in, token_out);
        if max_in.is_zero() {
            continue;
        }
        out.any_positive = true;
        if max_in >= floor {
            out.fills_sample = true;
        }
        if max_in > out.top {
            out.top = max_in;
        }
    }
    out
}

/// A snapshot can price a positive input.
///
/// A missing snapshot is kept. Zero reserves, an empty concentrated book, and
/// an uninitialized price are not. A one-wei pool is.
pub(crate) fn has_trade_size(kind: u8, id: B256, state: PoolState) -> bool {
    if state.immutable.is_none() || state.mutable.is_none() {
        return true;
    }
    let plan = plan_of(kind, id, state);
    if plan.missing.is_some() {
        return true;
    }
    if matches!(plan.detail, Some(PoolDetail::Failed(_))) {
        return true;
    }
    plan.directions.iter().any(|dir| !dir.max_in.is_zero())
}

pub(super) fn plan_of(kind: u8, id: B256, state: PoolState) -> PoolPlan {
    let (block, generation, truncated, tick_count, bitmap) = match &state.mutable {
        Some(row) => (
            row.block,
            row.generation,
            row.truncated,
            row.tick_count,
            row.bitmap,
        ),
        None => (0, 0, false, 0, B256::ZERO),
    };
    let (curve_version, skip_reason, fits) = curve_meta(state.curve.as_ref());
    let venue = state.venue.clone();
    let mut plan = PoolPlan {
        kind,
        id,
        block,
        generation,
        truncated,
        tick_count,
        bitmap,
        factory: venue.as_ref().map(|row| row.factory),
        identity_fee: venue.as_ref().and_then(|row| row.fee),
        identity_spacing: venue.as_ref().and_then(|row| row.tick_spacing),
        created_block: venue.as_ref().and_then(|row| row.block_number),
        canonical: None,
        curve_version,
        skip_reason,
        fits,
        pairs_capped: false,
        missing: None,
        detail: None,
        directions: Vec::new(),
    };
    let (immutable, mutable) = match (&state.immutable, &state.mutable) {
        (Some(immutable), Some(mutable)) => (immutable, mutable),
        _ => {
            plan.missing = Some("missing snapshot".into());
            return plan;
        }
    };
    let detail = match join_detail(immutable, &mutable.body, &state.ticks, kind, id) {
        Ok(detail) => detail,
        Err(err) => {
            plan.missing = Some(err);
            return plan;
        }
    };
    plan.canonical = Some(is_canonical(&detail));
    let (capped, pairs) = pairs_of(&detail);
    plan.pairs_capped = capped;
    plan.directions = pairs
        .into_iter()
        .map(|(token_in, token_out)| direction_of(&detail, token_in, token_out))
        .collect();
    plan.detail = Some(detail);
    plan
}

pub(super) fn curve_meta(curve: Option<&CurveRecord>) -> (Option<u32>, Option<u8>, Vec<Value>) {
    let Some(curve) = curve else {
        return (None, None, Vec::new());
    };
    let skip = if curve.payload.first() == Some(&1)
        && curve.payload.get(1).is_some_and(|reason| *reason != 0)
    {
        curve.payload.get(1).copied()
    } else {
        None
    };
    let fits = decode_fit(&curve.payload)
        .map(|fit| fit.directions.iter().map(fit_json).collect())
        .unwrap_or_default();
    (Some(curve.version), skip, fits)
}

pub(super) fn bigint_from_u256(value: U256) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &value.to_be_bytes::<32>())
}

pub(super) fn pairs_of(detail: &PoolDetail) -> (bool, Vec<(Address, Address)>) {
    let mut tokens = legs(detail);
    let capped = tokens.len() > MAX_LEGS;
    tokens.truncate(MAX_LEGS);
    let mut pairs = Vec::new();
    for (i, token_in) in tokens.iter().enumerate() {
        for (j, token_out) in tokens.iter().enumerate() {
            if i != j {
                pairs.push((*token_in, *token_out));
            }
        }
    }
    (capped, pairs)
}

pub(super) fn legs(detail: &PoolDetail) -> Vec<Address> {
    match detail {
        PoolDetail::ConstantProduct(pool) => vec![pool.token0, pool.token1],
        PoolDetail::Concentrated(pool) => vec![pool.token0, pool.token1],
        PoolDetail::Balancer(pool) => pool.tokens.clone(),
        PoolDetail::Curve(pool) => pool.coins.iter().map(|coin| coin.token).collect(),
        PoolDetail::Metric(pool) => vec![pool.token0, pool.token1],
        PoolDetail::Failed(_) => Vec::new(),
    }
}

pub(super) fn direction_of(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
) -> Direction {
    let (max_in, max_source) = domain_max(detail, token_in, token_out);
    let mut samples = Vec::new();
    if !max_in.is_zero() {
        for (fraction, amount) in grid_amounts(max_in) {
            samples.push(sample_at(
                detail,
                token_in,
                token_out,
                "grid",
                Some(fraction),
                amount,
            ));
        }
    }
    Direction {
        token_in,
        token_out,
        max_in,
        max_source,
        chain_max_in: None,
        larger_input_reverted: false,
        samples,
    }
}

pub(super) fn sample_at(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
    role: &'static str,
    fraction: Option<&'static str>,
    amount_in: U256,
) -> Sample {
    let mut sample = Sample {
        role,
        fraction,
        amount_in,
        local_out: None,
        local_fee: None,
        local_exhausted: None,
        local_error: None,
        chain_out: None,
        chain_error: None,
        quoter: None,
        method: None,
        attempted: false,
        alt: None,
        alt_method: None,
    };
    match detail.quote(token_in, token_out, amount_in) {
        Ok(quote) => fill_local(&mut sample, &quote),
        Err(err) => sample.local_error = Some(err),
    }
    sample
}

pub(super) fn fill_local(sample: &mut Sample, quote: &Quote) {
    sample.local_out = Some(quote.amount_out);
    sample.local_fee = Some(quote.fee_paid);
    sample.local_exhausted = Some(quote.exhausted);
}

/// Knots `estimate` accepts on one direction: the origin, the sampling ceiling,
/// and the grid fractions of that ceiling. A fraction `estimate` refuses is omitted.
pub(crate) fn direction_knots(
    detail: &PoolDetail,
    curve: &crate::state::CurveRecord,
    token_in: Address,
    token_out: Address,
) -> Vec<(BigInt, BigInt)> {
    let mut knots = vec![(BigInt::from(0), BigInt::from(0))];
    let (max_in, _) = domain_max(detail, token_in, token_out);
    if max_in.is_zero() {
        return knots;
    }
    for (_, amount) in grid_amounts(max_in) {
        let Ok(quote) =
            crate::state::estimate(curve, curve.generation, token_in, token_out, amount)
        else {
            continue;
        };
        knots.push((bigint_from_u256(amount), bigint_from_u256(quote.amount_out)));
    }
    knots
}
