//! Largest input that still has a price on one snapshot.
//!
//! Constant product stops at the input that buys 99% of the output reserve.
//! A concentrated book stops at the last ladder step. Balancer and Curve search
//! toward 99% of the output balance. Metric searches until the quote is exhausted.
//! The grid is fractions of that input.

use alloy_primitives::{Address, U256};
use chain::detail::{product_fee_pips, PoolDetail, Quote};

/// Fractions of the priced input, as a label and `num/den`.
pub(super) const GRID: &[(&str, u32, u32)] = &[
    ("0.0001", 1, 10_000),
    ("0.001", 1, 1_000),
    ("0.01", 1, 100),
    ("0.05", 1, 20),
    ("0.1", 1, 10),
    ("0.25", 1, 4),
    ("0.5", 1, 2),
    ("0.8", 4, 5),
    ("0.95", 19, 20),
    ("1", 1, 1),
];

pub(super) fn domain_max(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
) -> (U256, &'static str) {
    match detail {
        PoolDetail::ConstantProduct(pool) => cp_max(pool, token_in, token_out),
        PoolDetail::Concentrated(pool) => {
            let zero_for_one = token_in == pool.token0 && token_out == pool.token1;
            let one_for_zero = token_in == pool.token1 && token_out == pool.token0;
            if !zero_for_one && !one_for_zero {
                return (U256::ZERO, "none");
            }
            concentrated_max(pool, zero_for_one)
        }
        PoolDetail::Balancer(_) | PoolDetail::Curve(_) => {
            let Some((balance_in, balance_out)) = balances(detail, token_in, token_out) else {
                return (U256::ZERO, "none");
            };
            search_output(detail, token_in, token_out, balance_in, balance_out)
        }
        PoolDetail::Metric(pool) => metric_max(detail, pool, token_in, token_out),
        PoolDetail::Failed(_) => (U256::ZERO, "none"),
    }
}

pub(super) fn cp_max(
    pool: &chain::detail::ConstantProduct,
    token_in: Address,
    token_out: Address,
) -> (U256, &'static str) {
    let (reserve_in, reserve_out) = if token_in == pool.token0 && token_out == pool.token1 {
        (pool.reserve0, pool.reserve1)
    } else if token_in == pool.token1 && token_out == pool.token0 {
        (pool.reserve1, pool.reserve0)
    } else {
        return (U256::ZERO, "none");
    };
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return (U256::ZERO, "none");
    }
    let fee = product_fee_pips(pool.kind, pool.fee_pips);
    if fee >= 1_000_000 {
        return (U256::ZERO, "none");
    }
    let den = U256::from(1_000_000u32 - fee);
    let Some(scaled) = reserve_in
        .checked_mul(U256::from(99u32))
        .and_then(|value| value.checked_mul(U256::from(1_000_000u32)))
    else {
        return (U256::ZERO, "overflow");
    };
    ((scaled + den - U256::from(1u8)) / den, "cp_99")
}

pub(super) fn concentrated_max(
    pool: &chain::detail::Concentrated,
    zero_for_one: bool,
) -> (U256, &'static str) {
    if pool.sqrt_price_x96.is_zero() {
        return (U256::ZERO, "uninitialized");
    }
    let Ok(ladder) = chain::detail::concentrated_ladder(pool) else {
        return (U256::ZERO, "uninitialized");
    };
    let steps = if zero_for_one {
        &ladder.zero_for_one
    } else {
        &ladder.one_for_zero
    };
    match steps.last() {
        Some(step) if !step.input.is_zero() => (step.input, "ladder"),
        _ => (U256::ZERO, "empty_book"),
    }
}

pub(super) fn balances(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
) -> Option<(U256, U256)> {
    match detail {
        PoolDetail::Balancer(pool) => {
            let input = pool.tokens.iter().position(|token| *token == token_in)?;
            let output = pool.tokens.iter().position(|token| *token == token_out)?;
            Some((
                chain::detail::v3_raw_balance(pool, input).ok()?,
                chain::detail::v3_raw_balance(pool, output).ok()?,
            ))
        }
        PoolDetail::Curve(pool) => {
            let input = pool.coins.iter().position(|coin| coin.token == token_in)?;
            let output = pool.coins.iter().position(|coin| coin.token == token_out)?;
            Some((pool.coins[input].balance, pool.coins[output].balance))
        }
        _ => None,
    }
}

pub(super) fn search_output(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
    balance_in: U256,
    balance_out: U256,
) -> (U256, &'static str) {
    if balance_in.is_zero() || balance_out.is_zero() {
        return (U256::ZERO, "none");
    }
    if detail.quote(token_in, token_out, U256::from(1u8)).is_err() {
        return (balance_in, "balance");
    }
    let cap = balance_in.saturating_mul(U256::from(1000u32));
    let target = balance_out.saturating_mul(U256::from(99u8)) / U256::from(100u8);
    let found = bisect_local(detail, token_in, token_out, cap, |quote| {
        quote.exhausted || (!target.is_zero() && quote.amount_out >= target)
    });
    match found {
        Some(value) => (value, "balance_search"),
        None => (cap, "balance"),
    }
}

pub(super) fn metric_max(
    detail: &PoolDetail,
    pool: &chain::detail::MetricPool,
    token_in: Address,
    token_out: Address,
) -> (U256, &'static str) {
    let selling_token0 = token_in == pool.token0 && token_out == pool.token1;
    let selling_token1 = token_in == pool.token1 && token_out == pool.token0;
    if !selling_token0 && !selling_token1 {
        return (U256::ZERO, "none");
    }
    if detail.quote(token_in, token_out, U256::from(1u8)).is_err() {
        return (U256::ZERO, "none");
    }
    let bin = pool.bins.iter().find(|bin| bin.index == pool.bin_index);
    let (scale, scaled) = if selling_token0 {
        (pool.scale0, bin.map(|bin| bin.token0_scaled).unwrap_or(0))
    } else {
        (pool.scale1, bin.map(|bin| bin.token1_scaled).unwrap_or(0))
    };
    let base = if scaled == 0 || scale.is_zero() {
        U256::from(1_000_000_000_000_000_000u64)
    } else {
        U256::from(scaled) / scale
    };
    let base = if base.is_zero() {
        U256::from(1u8)
    } else {
        base
    };
    let cap = base.saturating_mul(U256::from(1000u32));
    match bisect_local(detail, token_in, token_out, cap, |quote| quote.exhausted) {
        Some(value) => (value, "balance_search"),
        None => (cap, "balance"),
    }
}

pub(super) fn bisect_local(
    detail: &PoolDetail,
    token_in: Address,
    token_out: Address,
    cap: U256,
    done: impl Fn(&Quote) -> bool,
) -> Option<U256> {
    if cap.is_zero() {
        return None;
    }
    let mut lo = U256::from(1u8);
    let mut hi = cap;
    let mut best = None;
    for _ in 0..256 {
        if lo >= hi {
            break;
        }
        let mid = lo + (hi - lo) / U256::from(2u8);
        if mid.is_zero() {
            break;
        }
        match detail.quote(token_in, token_out, mid) {
            Ok(quote) if done(&quote) => {
                best = Some(mid);
                hi = mid;
            }
            Ok(_) => lo = mid.saturating_add(U256::from(1u8)),
            Err(_) => {
                if mid <= lo {
                    break;
                }
                hi = mid;
            }
        }
    }
    best
}

pub(super) fn grid_amounts(max_in: U256) -> Vec<(&'static str, U256)> {
    let mut out: Vec<(&'static str, U256)> = Vec::new();
    for &(fraction, num, den) in GRID {
        let amount = match max_in.checked_mul(U256::from(num)) {
            Some(value) => value / U256::from(den),
            None => max_in,
        };
        if amount.is_zero() {
            continue;
        }
        if let Some(last) = out.last_mut() {
            if last.1 == amount {
                last.0 = fraction;
                continue;
            }
        }
        out.push((fraction, amount));
    }
    out
}
