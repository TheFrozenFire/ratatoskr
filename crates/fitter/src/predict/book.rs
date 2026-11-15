//! Concentrated liquidity and bin quotes.

use super::product::{div_up, mul_div_up};
use super::share::*;
use num_bigint::BigInt;
use num_traits::{One, Signed, Zero};
use std::collections::BTreeMap;

pub(crate) fn sqrt_at_tick(tick: &BigInt) -> Option<BigInt> {
    let abs_tick = tick.abs();
    if abs_tick > *MAX_TICK {
        return None;
    }
    let mut ratio = if (&abs_tick & BigInt::one()).is_zero() {
        BigInt::one() << 128u32
    } else {
        TICK_BITS[0].clone()
    };
    for bit in 1..20 {
        if !(&abs_tick & (BigInt::one() << (bit as u32))).is_zero() {
            ratio = (&ratio * &TICK_BITS[bit as usize]) >> 128u32;
        }
    }
    if tick.is_positive() {
        ratio = floor_div(&((BigInt::one() << 256u32) - 1), &ratio);
    }
    let mut rounded = &ratio >> 32u32;
    if !(&ratio & ((BigInt::one() << 32u32) - BigInt::one())).is_zero() {
        rounded += 1;
    }
    Some(rounded)
}

pub(crate) fn ordered_prices(left: &BigInt, right: &BigInt) -> (BigInt, BigInt) {
    if left < right {
        (left.clone(), right.clone())
    } else {
        (right.clone(), left.clone())
    }
}

pub(crate) fn amount0_delta(
    sqrt_a: &BigInt,
    sqrt_b: &BigInt,
    liquidity: &BigInt,
    round_up: bool,
) -> Option<BigInt> {
    let (lo, hi) = ordered_prices(sqrt_a, sqrt_b);
    if !lo.is_positive() {
        return None;
    }
    let numerator1 = liquidity << 96u32;
    let numerator2 = &hi - &lo;
    if round_up {
        let inner = mul_div_up(&numerator1, &numerator2, &hi)?;
        return div_up(&inner, &lo);
    }
    Some(floor_div(
        &floor_div(&(&numerator1 * &numerator2), &hi),
        &lo,
    ))
}

pub(crate) fn amount1_delta(
    sqrt_a: &BigInt,
    sqrt_b: &BigInt,
    liquidity: &BigInt,
    round_up: bool,
) -> Option<BigInt> {
    let (lo, hi) = ordered_prices(sqrt_a, sqrt_b);
    let diff = &hi - &lo;
    if round_up {
        mul_div_up(liquidity, &diff, &Q96)
    } else {
        Some(floor_div(&(liquidity * &diff), &Q96))
    }
}

pub(crate) fn next_sqrt_from_amount0(
    sqrt: &BigInt,
    liquidity: &BigInt,
    amount: &BigInt,
) -> Option<BigInt> {
    if amount.is_zero() {
        return Some(sqrt.clone());
    }
    let numerator1 = liquidity << 96u32;
    let denom = &numerator1 + amount * sqrt;
    mul_div_up(&numerator1, sqrt, &denom)
}

pub(crate) fn next_sqrt_from_amount1(
    sqrt: &BigInt,
    liquidity: &BigInt,
    amount: &BigInt,
) -> Option<BigInt> {
    if amount.is_zero() {
        return Some(sqrt.clone());
    }
    if !liquidity.is_positive() {
        return None;
    }
    Some(sqrt + floor_div(&(amount * &*Q96), liquidity))
}

pub(crate) fn apply_liquidity(liquidity: &BigInt, delta: &BigInt) -> Option<BigInt> {
    if delta.is_negative() {
        if liquidity < &-delta {
            return None;
        }
        return Some(liquidity + delta);
    }
    Some(liquidity + delta)
}

pub(crate) fn bisect_right(keys: &[BigInt], target: &BigInt) -> usize {
    let mut lo = 0;
    let mut hi = keys.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if &keys[mid] <= target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

pub(crate) fn next_initialized(
    keys: &[BigInt],
    current: &BigInt,
    include: bool,
    zero_for_one: bool,
) -> Option<BigInt> {
    if zero_for_one {
        let probe = if include {
            current.clone()
        } else {
            current - 1
        };
        let index = bisect_right(keys, &probe);
        if index == 0 {
            return None;
        }
        Some(keys[index - 1].clone())
    } else {
        let index = bisect_right(keys, current);
        if index >= keys.len() {
            return None;
        }
        Some(keys[index].clone())
    }
}

pub(crate) fn tick_net(
    nets: &BTreeMap<BigInt, BigInt>,
    boundary: &BigInt,
    zero_for_one: bool,
) -> Option<BigInt> {
    let net = nets.get(boundary)?.clone();
    if zero_for_one {
        Some(-net)
    } else {
        Some(net)
    }
}

pub(crate) fn concentrated_out(
    sqrt_price: &BigInt,
    liquidity: &BigInt,
    tick_start: &BigInt,
    fee_pips: &BigInt,
    ticks: &[(BigInt, BigInt)],
    zero_for_one: bool,
    amount: &BigInt,
) -> Option<BigInt> {
    let million = BigInt::from(1_000_000u32);
    if !sqrt_price.is_positive()
        || fee_pips.is_negative()
        || fee_pips > &million
        || amount.is_negative()
    {
        return None;
    }
    let mut ordered = ticks.to_vec();
    ordered.sort();
    let keys: Vec<BigInt> = ordered.iter().map(|(tick, _)| tick.clone()).collect();
    let mut nets = BTreeMap::new();
    for (tick, net) in &ordered {
        nets.insert(tick.clone(), net.clone());
    }
    let mut sqrt = sqrt_price.clone();
    let mut liq = liquidity.clone();
    let mut tick = tick_start.clone();
    let mut include = zero_for_one;
    let mut remaining = amount.clone();
    let mut amount_out = BigInt::zero();
    for _ in 0..ordered.len() + 2 {
        if !remaining.is_positive() {
            break;
        }
        let boundary = next_initialized(&keys, &tick, include, zero_for_one);
        include = false;
        let target = if let Some(bound) = &boundary {
            sqrt_at_tick(bound)?
        } else if zero_for_one {
            MIN_SQRT.clone()
        } else {
            MAX_SQRT.clone()
        };
        let on_tick = boundary.is_some() && target == sqrt;
        if !on_tick && ((zero_for_one && target >= sqrt) || (!zero_for_one && target <= sqrt)) {
            break;
        }
        if on_tick {
            let bound = boundary.clone()?;
            let net = tick_net(&nets, &bound, zero_for_one)?;
            liq = apply_liquidity(&liq, &net)?;
            tick = bound;
            continue;
        }
        if liq.is_zero() {
            let Some(bound) = boundary.clone() else {
                break;
            };
            let net = tick_net(&nets, &bound, zero_for_one)?;
            liq = apply_liquidity(&liq, &net)?;
            sqrt = target;
            tick = bound;
            continue;
        }
        let (in_to, out_full) = if zero_for_one {
            (
                amount0_delta(&target, &sqrt, &liq, true)?,
                amount1_delta(&target, &sqrt, &liq, false)?,
            )
        } else {
            (
                amount1_delta(&sqrt, &target, &liq, true)?,
                amount0_delta(&sqrt, &target, &liq, false)?,
            )
        };
        let less_fee = floor_div(&(&remaining * (&million - fee_pips)), &million);
        let reached;
        let step_in;
        let step_fee_paid;
        let step_out;
        let sqrt_next;
        if less_fee >= in_to {
            let step_fee = if fee_pips.is_zero() {
                Some(BigInt::zero())
            } else {
                mul_div_up(&in_to, fee_pips, &(&million - fee_pips))
            };
            if let Some(step_fee) = step_fee {
                if &in_to + &step_fee <= remaining {
                    reached = true;
                    step_in = in_to;
                    step_fee_paid = step_fee;
                    step_out = out_full;
                    sqrt_next = target;
                } else {
                    reached = false;
                    step_in = less_fee.clone();
                    step_fee_paid = &remaining - &less_fee;
                    sqrt_next = if zero_for_one {
                        next_sqrt_from_amount0(&sqrt, &liq, &less_fee)?
                    } else {
                        next_sqrt_from_amount1(&sqrt, &liq, &less_fee)?
                    };
                    step_out = if zero_for_one {
                        amount1_delta(&sqrt_next, &sqrt, &liq, false)?
                    } else {
                        amount0_delta(&sqrt, &sqrt_next, &liq, false)?
                    };
                }
            } else {
                reached = false;
                step_in = less_fee.clone();
                step_fee_paid = &remaining - &less_fee;
                sqrt_next = if zero_for_one {
                    next_sqrt_from_amount0(&sqrt, &liq, &less_fee)?
                } else {
                    next_sqrt_from_amount1(&sqrt, &liq, &less_fee)?
                };
                step_out = if zero_for_one {
                    amount1_delta(&sqrt_next, &sqrt, &liq, false)?
                } else {
                    amount0_delta(&sqrt, &sqrt_next, &liq, false)?
                };
            }
        } else {
            reached = false;
            step_in = less_fee.clone();
            step_fee_paid = &remaining - &less_fee;
            sqrt_next = if zero_for_one {
                next_sqrt_from_amount0(&sqrt, &liq, &less_fee)?
            } else {
                next_sqrt_from_amount1(&sqrt, &liq, &less_fee)?
            };
            step_out = if zero_for_one {
                amount1_delta(&sqrt_next, &sqrt, &liq, false)?
            } else {
                amount0_delta(&sqrt, &sqrt_next, &liq, false)?
            };
        }
        if &step_in + &step_fee_paid > remaining {
            return None;
        }
        remaining -= &step_in + &step_fee_paid;
        amount_out += &step_out;
        sqrt = sqrt_next;
        if boundary.is_none() || !reached {
            break;
        }
        let bound = boundary?;
        let net = tick_net(&nets, &bound, zero_for_one)?;
        liq = apply_liquidity(&liq, &net)?;
        tick = bound;
    }
    Some(amount_out)
}

pub(crate) fn bin_out(
    reference_x64: &BigInt,
    distance_e6: &BigInt,
    length_e6: &BigInt,
    position: &BigInt,
    amount: &BigInt,
    selling_token0: bool,
    scaled_out: &BigInt,
    scale_out: &BigInt,
) -> Option<BigInt> {
    if !reference_x64.is_positive() || amount.is_negative() {
        return None;
    }
    if scale_out.is_positive() && !scaled_out.is_positive() {
        return Some(BigInt::zero());
    }
    let million = BigInt::from(1_000_000u32);
    let lower = if distance_e6 >= &BigInt::zero() {
        floor_div(&(reference_x64 * (&million + distance_e6)), &million)
    } else {
        let distance = -distance_e6;
        if distance >= million {
            return None;
        }
        floor_div(&(reference_x64 * (&million - &distance)), &million)
    };
    let upper_distance = distance_e6 + length_e6;
    let upper = if upper_distance >= BigInt::zero() {
        floor_div(&(reference_x64 * (&million + &upper_distance)), &million)
    } else {
        let distance = -&upper_distance;
        if distance >= million {
            return None;
        }
        floor_div(&(reference_x64 * (&million - &distance)), &million)
    };
    if upper < lower {
        return None;
    }
    let span = &upper - &lower;
    let max_pos = (BigInt::one() << 104u32) - 1;
    let price = if span.is_zero() || !position.is_positive() {
        lower.clone()
    } else {
        &lower + floor_div(&(&span * position), &max_pos)
    };
    let mut out = if selling_token0 {
        floor_div(&(amount * &price), &(BigInt::one() << 64u32))
    } else if !price.is_positive() {
        return None;
    } else {
        floor_div(&(amount * (BigInt::one() << 64u32)), &price)
    };
    if scale_out.is_positive() && scaled_out.is_positive() {
        let available = floor_div(scaled_out, scale_out);
        if out > available {
            out = available;
        }
    }
    Some(out)
}
