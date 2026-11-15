use super::super::types::*;
use super::mul_div::*;
use alloy_primitives::{Address, U256};
use std::str::FromStr;

const MIN_SQRT: &str = "4295128739";
const MAX_SQRT: &str = "1461446703485210103287273052203988822378723970342";
const MAX_TICK: i32 = 887272;

pub(super) fn quote_concentrated(
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
    let mut include_tick = zero_for_one;
    let mut remaining = amount_in;
    let mut amount_out = U256::ZERO;
    let mut fee_paid = U256::ZERO;
    let steps = pool.ticks.len() + 1;
    for _ in 0..=steps {
        if remaining.is_zero() {
            break;
        }
        let boundary = next_initialized(pool, tick, zero_for_one, include_tick);
        include_tick = false;
        let target = match boundary {
            Some(next) => sqrt_at_tick(next)?,
            None if zero_for_one => min_sqrt(),
            None => max_sqrt(),
        };
        // A price already on an initialized tick still has to cross it. The
        // snapshot's liquidity includes that tick.
        let on_tick = boundary.is_some() && target == sqrt;
        if !on_tick && (zero_for_one && target >= sqrt || !zero_for_one && target <= sqrt) {
            break;
        }
        if on_tick {
            let Some(crossed) = boundary else {
                break;
            };
            liquidity = add_liquidity(liquidity, net_on_cross(pool, crossed, zero_for_one))?;
            tick = crossed;
            continue;
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

pub(super) fn next_initialized(
    pool: &Concentrated,
    tick: i32,
    zero_for_one: bool,
    include_tick: bool,
) -> Option<i32> {
    if zero_for_one {
        pool.ticks
            .iter()
            .filter(|word| {
                if include_tick {
                    word.tick <= tick
                } else {
                    word.tick < tick
                }
            })
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

pub(super) fn net_on_cross(pool: &Concentrated, tick: i32, zero_for_one: bool) -> i128 {
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

pub(super) fn add_liquidity(liquidity: u128, delta: i128) -> Result<u128, String> {
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

pub(super) fn amount0_delta(
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

pub(super) fn amount1_delta(
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

pub(super) fn next_sqrt_from_amount0(
    sqrt: U256,
    liquidity: u128,
    amount: U256,
) -> Result<U256, String> {
    if amount.is_zero() {
        return Ok(sqrt);
    }
    let numerator1 = U256::from(liquidity) << 96;
    let denom = u512(numerator1)
        .checked_add(widening(amount, sqrt))
        .ok_or_else(|| "sqrt".to_string())?;
    mul_div_up_512(numerator1, sqrt, denom)
}

pub(super) fn next_sqrt_from_amount1(
    sqrt: U256,
    liquidity: u128,
    amount: U256,
) -> Result<U256, String> {
    if amount.is_zero() {
        return Ok(sqrt);
    }
    if liquidity == 0 {
        return Err("liquidity".into());
    }
    let quotient = mul_div(amount, q96(), U256::from(liquidity))?;
    sqrt.checked_add(quotient).ok_or_else(|| "sqrt".to_string())
}

pub(crate) fn sqrt_at_tick(tick: i32) -> Result<U256, String> {
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

pub(super) fn tick_factor(bit: u32) -> U256 {
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

pub(super) fn min_sqrt() -> U256 {
    U256::from_str(MIN_SQRT).expect("min sqrt")
}

pub(super) fn max_sqrt() -> U256 {
    U256::from_str(MAX_SQRT).expect("max sqrt")
}

pub(super) fn u256_hex(text: &str) -> U256 {
    U256::from_str_radix(text, 16).expect("hex")
}

pub(super) fn ordered(left: U256, right: U256) -> (U256, U256) {
    if left < right {
        (left, right)
    } else {
        (right, left)
    }
}

/// One priced span of a concentrated pool, with the cumulative input that
/// reaches its end. The partial span uses the same `swap_step` as `quote`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LadderStep {
    pub liquidity: u128,
    pub sqrt_start: U256,
    pub sqrt_end: U256,
    pub tick_end: i32,
    pub input: U256,
    pub output: U256,
    pub fee: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConcentratedLadder {
    pub zero_for_one: Vec<LadderStep>,
    pub one_for_zero: Vec<LadderStep>,
}

pub fn concentrated_ladder(pool: &Concentrated) -> Result<ConcentratedLadder, String> {
    Ok(ConcentratedLadder {
        zero_for_one: walk_ladder(pool, true)?,
        one_for_zero: walk_ladder(pool, false)?,
    })
}

// The stored curve no longer embeds the ladder. The concentrated quote test
// still prices spans through this helper.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn quote_ladder(
    pool: &Concentrated,
    ladder: &ConcentratedLadder,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    if amount_in.is_zero() {
        return Ok(Quote {
            amount_in,
            amount_out: U256::ZERO,
            fee_paid: U256::ZERO,
            exhausted: false,
        });
    }
    let fee = pool.fee_pips.ok_or_else(|| "fee unread".to_string())?;
    let zero_for_one = if token_in == pool.token0 && token_out == pool.token1 {
        true
    } else if token_in == pool.token1 && token_out == pool.token0 {
        false
    } else {
        return Err("token".into());
    };
    let steps = if zero_for_one {
        &ladder.zero_for_one
    } else {
        &ladder.one_for_zero
    };
    let Some(last) = steps.last() else {
        return Err("ladder".into());
    };
    let mut previous_in = U256::ZERO;
    let mut previous_out = U256::ZERO;
    let mut previous_fee = U256::ZERO;
    for step in steps {
        if amount_in > step.input {
            previous_in = step.input;
            previous_out = step.output;
            previous_fee = step.fee;
            continue;
        }
        if amount_in == step.input {
            return Ok(Quote {
                amount_in,
                amount_out: step.output,
                fee_paid: step.fee,
                exhausted: false,
            });
        }
        let partial = swap_step(
            step.sqrt_start,
            step.sqrt_end,
            step.liquidity,
            amount_in - previous_in,
            fee,
            zero_for_one,
        )?;
        return Ok(Quote {
            amount_in: previous_in + partial.amount_in + partial.fee,
            amount_out: previous_out + partial.amount_out,
            fee_paid: previous_fee + partial.fee,
            exhausted: false,
        });
    }
    Ok(Quote {
        amount_in: last.input,
        amount_out: last.output,
        fee_paid: last.fee,
        exhausted: true,
    })
}

pub(super) fn walk_ladder(
    pool: &Concentrated,
    zero_for_one: bool,
) -> Result<Vec<LadderStep>, String> {
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
    let mut include_tick = zero_for_one;
    let mut input = U256::ZERO;
    let mut output = U256::ZERO;
    let mut fee_paid = U256::ZERO;
    let mut steps = Vec::new();
    let limit = pool.ticks.len() + 1;
    for _ in 0..=limit {
        let boundary = next_initialized(pool, tick, zero_for_one, include_tick);
        include_tick = false;
        let (target, tick_end) = match boundary {
            Some(next) => (sqrt_at_tick(next)?, next),
            None if zero_for_one => (min_sqrt(), -MAX_TICK),
            None => (max_sqrt(), MAX_TICK),
        };
        // A price already on an initialized tick still has to cross it. The
        // snapshot's liquidity includes that tick.
        let on_tick = boundary.is_some() && target == sqrt;
        if !on_tick && (zero_for_one && target >= sqrt || !zero_for_one && target <= sqrt) {
            break;
        }
        if on_tick {
            let Some(crossed) = boundary else {
                break;
            };
            liquidity = add_liquidity(liquidity, net_on_cross(pool, crossed, zero_for_one))?;
            tick = crossed;
            continue;
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
        let sqrt_start = sqrt;
        let (spent, gained, step_fee) = cost_to_target(sqrt, target, liquidity, fee, zero_for_one)?;
        input = input.checked_add(spent).ok_or("input")?;
        output = output.checked_add(gained).ok_or("output")?;
        fee_paid = fee_paid.checked_add(step_fee).ok_or("fee")?;
        steps.push(LadderStep {
            liquidity,
            sqrt_start,
            sqrt_end: target,
            tick_end,
            input,
            output,
            fee: fee_paid,
        });
        let Some(crossed) = boundary else {
            break;
        };
        liquidity = add_liquidity(liquidity, net_on_cross(pool, crossed, zero_for_one))?;
        sqrt = target;
        tick = crossed;
    }
    Ok(steps)
}

pub(super) fn cost_to_target(
    sqrt: U256,
    target: U256,
    liquidity: u128,
    fee: u32,
    zero_for_one: bool,
) -> Result<(U256, U256, U256), String> {
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
    let fee_word = U256::from(fee);
    let denom = U256::from(1_000_000u64);
    let step_fee = if fee_word.is_zero() {
        U256::ZERO
    } else {
        mul_div_up(in_to_target, fee_word, denom - fee_word)?
    };
    let spent = in_to_target.checked_add(step_fee).ok_or("input")?;
    Ok((spent, out_to_target, step_fee))
}
