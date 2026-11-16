use super::super::types::*;
use super::curve::stable_out;
use super::mul_div::*;
use super::product::constant_product_out;
use alloy_primitives::{Address, U256};
use store_model::BALANCER_V3;

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
    let mut quote = if pool.kind != BALANCER_V3 {
        quote_balances(pool, input, output, &pool.balances, amount_in, fee, one)?
    } else {
        let (balances, amount_in, out_scale, out_rate, in_scale, in_rate) =
            balancer_space(pool, input, output, amount_in)?;
        let mut quote = quote_balances(pool, input, output, &balances, amount_in, fee, one)?;
        quote.amount_out = unscale(quote.amount_out, out_scale, out_rate)?;
        quote.fee_paid = unscale(quote.fee_paid, in_scale, in_rate)?;
        quote
    };
    quote.amount_in = raw_in;
    Ok(quote)
}

fn quote_balances(
    pool: &BalancerPool,
    input: usize,
    output: usize,
    balances: &[U256],
    amount_in: U256,
    fee: U256,
    one: U256,
) -> Result<Quote, String> {
    if pool.flags & FLAG_STABLE != 0 {
        if pool.amp_precision.is_zero() {
            return Err("amp".into());
        }
        let amp = pool.amp / pool.amp_precision;
        return stable_out(balances, amp, input, output, amount_in, fee, one);
    }
    if pool.weights.len() != pool.tokens.len() {
        return Err("weights".into());
    }
    if pool.weights[input] == pool.weights[output] && !pool.weights[input].is_zero() {
        return constant_product_out(balances[input], balances[output], amount_in, fee, one);
    }
    weighted_out(
        balances[input],
        balances[output],
        pool.weights[input],
        pool.weights[output],
        amount_in,
        fee,
    )
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
        return Err("kind".into());
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
pub fn v3_raw_balance(pool: &BalancerPool, index: usize) -> Result<U256, String> {
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

pub(super) fn f64_of(value: U256) -> Result<f64, String> {
    // Decimal text parsed as f64 rounds the exact integer, ties to even.
    if let Ok(small) = u128::try_from(value) {
        return Ok(small as f64);
    }
    let bits = 256 - value.leading_zeros();
    let shift = bits - 53;
    let mut mant = (value >> shift).as_limbs()[0];
    let remainder = value & ((U256::from(1u8) << shift) - U256::from(1u8));
    let half = U256::from(1u8) << (shift - 1);
    let round_up = remainder > half || (remainder == half && mant & 1 == 1);
    if round_up {
        mant += 1;
        if mant == 1u64 << 53 {
            mant >>= 1;
            return Ok(f64_from_sig(mant, bits as u32));
        }
    }
    Ok(f64_from_sig(mant, (bits - 1) as u32))
}

fn f64_from_sig(mant: u64, exp: u32) -> f64 {
    if exp >= 1024 {
        return f64::INFINITY;
    }
    f64::from_bits(((u64::from(exp) + 1023) << 52) | (mant & ((1u64 << 52) - 1)))
}

pub(super) fn f64_floor(value: f64) -> Result<U256, String> {
    // `{value:.0}` rounds half to even, then prints that integer.
    if !value.is_finite() || value.is_sign_negative() {
        return Err("quote".into());
    }
    let base = value.floor();
    let diff = value - base;
    let rounded = if diff > 0.5 {
        base + 1.0
    } else if diff < 0.5 || (base as i64) % 2 == 0 {
        base
    } else {
        base + 1.0
    };
    if rounded == 0.0 {
        return Ok(U256::ZERO);
    }
    let bits = rounded.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1023;
    let mant = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
    if exp < 52 {
        return Ok(U256::from(mant >> (52 - exp)));
    }
    if exp > 255 {
        return Err("quote".into());
    }
    Ok(U256::from(mant) << (exp as u32 - 52))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn oracle_f64_of(value: U256) -> f64 {
        value.to_string().parse::<f64>().unwrap()
    }

    fn oracle_f64_floor(value: f64) -> U256 {
        U256::from_str(&format!("{value:.0}")).unwrap()
    }

    #[test]
    fn float_conversion_matches_decimal_text() {
        let mut values = vec![
            U256::ZERO,
            U256::from(1u8),
            U256::from(1u64 << 53),
            U256::from((1u64 << 53) + 1),
            U256::from((1u64 << 53) + 2),
            U256::from(1_000_000_000_000_000_000u64),
            U256::from(1_000_000_000_000_000_000_000u128),
            U256::from(4_000_000_000_000_000_000_000u128),
            U256::from(u128::MAX),
            U256::from(u128::MAX) + U256::from(1u8),
        ];
        values.push(U256::from(1u8) << 200);
        values.push((U256::from(1u8) << 200) + U256::from(1u8));
        for value in values {
            assert_eq!(f64_of(value).unwrap(), oracle_f64_of(value));
        }
        let mut half = 0.5f64;
        for _ in 0..20 {
            assert_eq!(f64_floor(half).unwrap(), oracle_f64_floor(half));
            half += 1.0;
        }
        for value in [0.0, 0.49, 1.0, 1.5, 2.5, 10.4, 1e18, 1e21] {
            assert_eq!(f64_floor(value).unwrap(), oracle_f64_floor(value));
        }
    }
}
