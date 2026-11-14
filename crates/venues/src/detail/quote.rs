//! Local quote of one ingested pool snapshot.

use super::*;
use crate::fit_math::{crypto2_out, crypto3_out};
use num_bigint::{BigInt, Sign};

pub(super) fn quote_product(
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

pub(crate) fn product_fee_pips(kind: u8, raw: Option<u32>) -> u32 {
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

pub(super) fn constant_product_out(
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

pub(super) fn sqrt_at_tick(tick: i32) -> Result<U256, String> {
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

pub(super) fn quote_balancer(
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
    let raw_in = amount_in;
    let (balances, amount_in, out_scale, out_rate, in_scale, in_rate) =
        balancer_space(pool, input, output, amount_in)?;
    let mut quote = if pool.flags & FLAG_STABLE != 0 {
        if pool.amp_precision.is_zero() {
            return Err("amp".into());
        }
        let amp = pool.amp / pool.amp_precision;
        stable_out(&balances, amp, input, output, amount_in, fee, one)?
    } else if pool.weights.len() != pool.tokens.len() {
        return Err("weights".into());
    } else if pool.weights[input] == pool.weights[output] && !pool.weights[input].is_zero() {
        constant_product_out(balances[input], balances[output], amount_in, fee, one)?
    } else {
        weighted_out(
            balances[input],
            balances[output],
            pool.weights[input],
            pool.weights[output],
            amount_in,
            fee,
        )?
    };
    if pool.kind == BALANCER_V3 {
        quote.amount_out = unscale(quote.amount_out, out_scale, out_rate)?;
        quote.fee_paid = unscale(quote.fee_paid, in_scale, in_rate)?;
    }
    quote.amount_in = raw_in;
    Ok(quote)
}

/// Invariant balances, scaled input, then the output scale, output rate, input scale, and input rate.
type BalancerSpace = (Vec<U256>, U256, U256, U256, U256, U256);

/// `getCurrentLiveBalances` is already `raw * scaling * rate / 1e18`.
/// The input is brought into that space and the output is scaled back to raw.
/// Multiplying the live balance by the rate again inflates the invariant.
fn balancer_space(
    pool: &BalancerPool,
    input: usize,
    output: usize,
    amount_in: U256,
) -> Result<BalancerSpace, String> {
    if pool.kind != BALANCER_V3 {
        return Ok((
            pool.balances.clone(),
            amount_in,
            U256::from(1u8),
            e18(),
            U256::from(1u8),
            e18(),
        ));
    }
    let n = pool.tokens.len();
    if pool.scaling.len() != n {
        return Err("scaling".into());
    }
    if !pool.rates.is_empty() && pool.rates.len() != n {
        return Err("rate".into());
    }
    if pool.scaling.iter().any(|scale| scale.is_zero()) {
        return Err("scaling".into());
    }
    let rate_in = token_rate(pool, input);
    let scaled = mul_div(amount_in, pool.scaling[input], U256::from(1u8))?;
    let scaled = mul_div(scaled, rate_in, e18())?;
    Ok((
        pool.balances.clone(),
        scaled,
        pool.scaling[output],
        token_rate(pool, output),
        pool.scaling[input],
        rate_in,
    ))
}

/// Live v3 balance back to raw token units. Other kinds already store raw balances.
pub(crate) fn v3_raw_balance(pool: &BalancerPool, index: usize) -> Result<U256, String> {
    if pool.kind != BALANCER_V3 || pool.scaling.len() != pool.tokens.len() {
        return Ok(pool.balances[index]);
    }
    unscale(
        pool.balances[index],
        pool.scaling[index],
        token_rate(pool, index),
    )
}

fn token_rate(pool: &BalancerPool, index: usize) -> U256 {
    if pool.rates.len() == pool.tokens.len() && !pool.rates[index].is_zero() {
        pool.rates[index]
    } else {
        e18()
    }
}

fn unscale(amount: U256, scaling: U256, rate: U256) -> Result<U256, String> {
    let rate = if rate.is_zero() { e18() } else { rate };
    let den = scaling
        .checked_mul(rate)
        .ok_or_else(|| "scale".to_string())?;
    mul_div(amount, e18(), den)
}

pub(super) fn weighted_out(
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

pub(super) fn quote_curve(
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

pub(super) fn quote_crypto(
    pool: &CurvePool,
    input: usize,
    output: usize,
    amount_in: U256,
) -> Result<Quote, String> {
    let n = pool.coins.len();
    if n != 2 && n != 3 {
        return Err("coins".into());
    }
    let amp_limit = U256::from(1_000_000_000_000u64);
    if pool.amp.is_zero() || pool.amp > amp_limit {
        return Err("amp".into());
    }
    if pool.coins.iter().any(|coin| coin.precision.is_zero()) {
        return Err("precision".into());
    }
    let balances = pool
        .coins
        .iter()
        .map(|coin| bigint_from_u256(coin.balance))
        .collect::<Vec<_>>();
    let precs = pool
        .coins
        .iter()
        .map(|coin| bigint_from_u256(coin.precision))
        .collect::<Vec<_>>();
    let dx = bigint_from_u256(amount_in);
    let amp = bigint_from_u256(pool.amp);
    let gamma = bigint_from_u256(pool.gamma);
    let d = bigint_from_u256(pool.d);
    let mid = bigint_from_u256(pool.mid_fee);
    let out_fee = bigint_from_u256(pool.out_fee);
    let fee_gamma = bigint_from_u256(pool.fee_gamma);
    let amount = if n == 3 {
        let scales = vec![
            bigint_from_u256(pool.coins[0].price_scale),
            bigint_from_u256(pool.coins[1].price_scale),
        ];
        crypto3_out(
            &balances,
            &precs,
            &scales,
            &amp,
            &gamma,
            &d,
            input as i64,
            output as i64,
            &dx,
            &mid,
            &out_fee,
            &fee_gamma,
        )
    } else {
        crypto2_out(
            &balances,
            &precs,
            &bigint_from_u256(pool.coins[0].price_scale),
            &amp,
            &gamma,
            &d,
            input as i64,
            output as i64,
            &dx,
            &mid,
            &out_fee,
            &fee_gamma,
        )
    };
    let amount_out = amount.ok_or_else(|| "quote".to_string())?;
    Ok(Quote {
        amount_in,
        amount_out: u256_from_bigint(&amount_out)?,
        fee_paid: U256::ZERO,
        exhausted: false,
    })
}

fn bigint_from_u256(value: U256) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &value.to_be_bytes::<32>())
}

fn u256_from_bigint(value: &BigInt) -> Result<U256, String> {
    if value.sign() == Sign::Minus {
        return Err("quote".into());
    }
    let bytes = value.to_bytes_be().1;
    if bytes.len() > 32 {
        return Err("quote".into());
    }
    let mut wide = [0u8; 32];
    wide[32 - bytes.len()..].copy_from_slice(&bytes);
    Ok(U256::from_be_bytes(wide))
}

pub(super) fn stable_out(
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

pub(super) fn get_d(xp: &[U256], amp: U256) -> Result<U256, String> {
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

pub(super) fn get_y(
    i: usize,
    j: usize,
    x: U256,
    xp: &[U256],
    amp: U256,
    d: U256,
) -> Result<U256, String> {
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

pub(super) fn xp_balance(coin: &CurveCoin) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(coin.balance)
    } else {
        mul_div(coin.balance, coin.rate, e18())
    }
}

pub(super) fn xp_amount(coin: &CurveCoin, amount: U256) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(amount)
    } else {
        mul_div(amount, coin.rate, e18())
    }
}

pub(super) fn raw_amount(coin: &CurveCoin, amount_xp: U256) -> Result<U256, String> {
    if coin.rate.is_zero() {
        Ok(amount_xp)
    } else {
        mul_div(amount_xp, e18(), coin.rate)
    }
}

pub(super) fn quote_metric(
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
    let (scale_out, scaled_out, scale_in) = if selling_token0 {
        (
            pool.scale1,
            bin.map(|bin| bin.token1_scaled).unwrap_or(0),
            pool.scale0,
        )
    } else {
        (
            pool.scale0,
            bin.map(|bin| bin.token0_scaled).unwrap_or(0),
            pool.scale1,
        )
    };
    if scale_in.is_zero() || scale_out.is_zero() {
        return Err("scale".into());
    }
    // An empty bin has no inventory. A zero scaled balance pays nothing.
    if scaled_out == 0 {
        return Ok(Quote {
            amount_in,
            amount_out: U256::ZERO,
            fee_paid: U256::ZERO,
            exhausted: true,
        });
    }
    // `price` is whole token1 per whole token0. `scale` is `10^(D - decimals)`.
    let unscaled = if selling_token0 {
        mul_div(amount_in, price, U256::from(1u8) << 64)?
    } else if price.is_zero() {
        return Err("price".into());
    } else {
        mul_div(amount_in, U256::from(1u8) << 64, price)?
    };
    let amount_out = mul_div(unscaled, scale_in, scale_out)?;
    let raw_available = Some(U256::from(scaled_out) / scale_out);
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

pub(super) fn distance_price(reference: U256, distance_e6: i32) -> Result<U256, String> {
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

pub(super) fn mul_div(a: U256, b: U256, denom: U256) -> Result<U256, String> {
    if denom.is_zero() {
        return Err("div0".into());
    }
    let (q, _) = widening(a, b).div_rem(u512(denom));
    narrow(q).ok_or_else(|| "overflow".to_string())
}

pub(super) fn mul_div_up(a: U256, b: U256, denom: U256) -> Result<U256, String> {
    mul_div_up_512(a, b, u512(denom))
}

pub(super) fn mul_div_up_512(a: U256, b: U256, denom: U512) -> Result<U256, String> {
    if denom.is_zero() {
        return Err("div0".into());
    }
    let (mut q, r) = widening(a, b).div_rem(denom);
    if !r.is_zero() {
        q += U512::from(1u8);
    }
    narrow(q).ok_or_else(|| "overflow".to_string())
}

pub(super) fn div_up(value: U256, denom: U256) -> Result<U256, String> {
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

pub(super) fn widening(a: U256, b: U256) -> U512 {
    a.widening_mul(b)
}

pub(super) fn mul_shift_128(a: U256, b: U256) -> U256 {
    narrow(widening(a, b) >> 128).expect("tick factor")
}

pub(super) fn u512(value: U256) -> U512 {
    let limbs = value.as_limbs();
    U512::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3], 0, 0, 0, 0])
}

pub(super) fn narrow(value: U512) -> Option<U256> {
    let limbs = value.as_limbs();
    if limbs[4] != 0 || limbs[5] != 0 || limbs[6] != 0 || limbs[7] != 0 {
        return None;
    }
    Some(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

pub(super) fn q96() -> U256 {
    U256::from(1u8) << 96
}

pub(super) fn e18() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}

pub(super) fn f64_of(value: U256) -> Result<f64, String> {
    value
        .to_string()
        .parse::<f64>()
        .map_err(|_| "scale".to_string())
}

pub(super) fn f64_floor(value: f64) -> Result<U256, String> {
    if !value.is_finite() || value < 0.0 {
        return Err("quote".into());
    }
    U256::from_str(&format!("{value:.0}")).map_err(|_| "quote".to_string())
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
pub(crate) struct LadderStep {
    pub liquidity: u128,
    pub sqrt_start: U256,
    pub sqrt_end: U256,
    pub tick_end: i32,
    pub input: U256,
    pub output: U256,
    pub fee: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConcentratedLadder {
    pub zero_for_one: Vec<LadderStep>,
    pub one_for_zero: Vec<LadderStep>,
}

pub(crate) fn concentrated_ladder(pool: &Concentrated) -> Result<ConcentratedLadder, String> {
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
