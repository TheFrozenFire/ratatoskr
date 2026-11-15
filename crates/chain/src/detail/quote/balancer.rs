use super::super::types::*;
use super::curve::stable_out;
use super::mul_div::*;
use super::product::constant_product_out;
use alloy_primitives::{Address, U256};
use std::str::FromStr;
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
