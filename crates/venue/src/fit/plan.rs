//! Local sample of one pool snapshot.
//!
//! A direction's domain is the largest input that still has a price. The grid
//! is fractions of that input. Each sample carries this crate's quote of the
//! ingested snapshot.

use super::domain::{domain_max, grid_amounts};
use crate::state::{join_detail, PoolState};
use alloy_primitives::{Address, B256, U256};
use chain::detail::{PoolDetail, Quote};
use num_bigint::{BigInt, Sign};

pub(super) const MAX_LEGS: usize = 4;
pub(super) const PAGE: usize = 32;

pub(super) struct Sample {
    pub(super) role: &'static str,
    pub(super) fraction: Option<&'static str>,
    pub(super) amount_in: U256,
    pub(super) local_out: Option<U256>,
}

pub(super) struct Direction {
    pub(super) token_in: Address,
    pub(super) token_out: Address,
    pub(super) max_in: U256,
    pub(super) samples: Vec<Sample>,
}

pub(super) struct PoolPlan {
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
    let pairs = pairs_of(detail);
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
    let (immutable, mutable) = match (&state.immutable, &state.mutable) {
        (Some(immutable), Some(mutable)) => (immutable, mutable),
        _ => {
            return PoolPlan {
                missing: Some("missing snapshot".into()),
                detail: None,
                directions: Vec::new(),
            };
        }
    };
    let detail = match join_detail(immutable, &mutable.body, &state.ticks, kind, id) {
        Ok(detail) => detail,
        Err(err) => {
            return PoolPlan {
                missing: Some(err),
                detail: None,
                directions: Vec::new(),
            };
        }
    };
    let pairs = pairs_of(&detail);
    let directions = pairs
        .into_iter()
        .map(|(token_in, token_out)| direction_of(&detail, token_in, token_out))
        .collect();
    PoolPlan {
        missing: None,
        detail: Some(detail),
        directions,
    }
}

pub(super) fn bigint_from_u256(value: U256) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &value.to_be_bytes::<32>())
}

pub(super) fn pairs_of(detail: &PoolDetail) -> Vec<(Address, Address)> {
    let mut tokens = legs(detail);
    tokens.truncate(MAX_LEGS);
    let mut pairs = Vec::new();
    for (i, token_in) in tokens.iter().enumerate() {
        for (j, token_out) in tokens.iter().enumerate() {
            if i != j {
                pairs.push((*token_in, *token_out));
            }
        }
    }
    pairs
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
    let (max_in, _) = domain_max(detail, token_in, token_out);
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
    };
    if let Ok(quote) = detail.quote(token_in, token_out, amount_in) {
        fill_local(&mut sample, &quote);
    }
    sample
}

pub(super) fn fill_local(sample: &mut Sample, quote: &Quote) {
    sample.local_out = Some(quote.amount_out);
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
        let Ok(quote) = super::estimate(curve, curve.generation, token_in, token_out, amount)
        else {
            continue;
        };
        knots.push((bigint_from_u256(amount), bigint_from_u256(quote.amount_out)));
    }
    knots
}
