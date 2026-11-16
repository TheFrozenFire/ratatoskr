//! Quote one direction of a stored curve.

use crate::state::{CurveRecord, CURVE_VERSION};
use alloy_primitives::{Address, B256, U256};
use chain::detail::Quote;
use num_bigint::{BigInt, Sign};
use num_traits::Zero;

pub fn estimate(
    curve: &CurveRecord,
    generation: u64,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    if curve.version != CURVE_VERSION || curve.generation != generation {
        return Err("curve generation".into());
    }
    estimate_payload(
        &curve.payload,
        curve.kind,
        curve.id,
        token_in,
        token_out,
        amount_in,
    )
}

fn estimate_payload(
    payload: &[u8],
    _kind: u8,
    _id: B256,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    let fit = fitter::decode_fit(payload)?;
    if fit.reason != 0 {
        return Err("not canonical".into());
    }
    let Some(direction) = fit
        .directions
        .iter()
        .find(|item| item.token_in == token_in && item.token_out == token_out)
    else {
        return Err("no quote".into());
    };
    if amount_in.is_zero() {
        return Ok(Quote {
            amount_in,
            amount_out: U256::ZERO,
            fee_paid: U256::ZERO,
            exhausted: false,
        });
    }
    let input = bigint_from_u256(amount_in);
    let output =
        fitter::eval_formula(&direction.formula, &input).ok_or_else(|| "no quote".to_string())?;
    Ok(Quote {
        amount_in,
        amount_out: u256_from_bigint(&output)?,
        fee_paid: u256_from_bigint(&fee_paid_of(&direction.formula, &input))?,
        exhausted: false,
    })
}

/// The fee a product quote withholds from the input.
fn fee_paid_of(formula: &fitter::Formula, amount: &BigInt) -> BigInt {
    match formula {
        fitter::Formula::Product { fee, denom, .. } if !denom.is_zero() && fee <= denom => {
            amount - (amount * (denom - fee)) / denom
        }
        _ => BigInt::from(0),
    }
}

fn bigint_from_u256(value: U256) -> BigInt {
    match u128::try_from(value) {
        Ok(small) => BigInt::from(small),
        Err(_) => BigInt::from_bytes_le(Sign::Plus, &value.to_le_bytes::<32>()),
    }
}

pub(super) fn accept_output(value: &BigInt) -> Result<(), String> {
    u256_from_bigint(value).map(|_| ())
}

fn u256_from_bigint(value: &BigInt) -> Result<U256, String> {
    if value.sign() == Sign::Minus {
        return Err("quote".into());
    }
    let mut limbs = [0u64; 4];
    for (index, digit) in value.magnitude().iter_u64_digits().enumerate() {
        if index == 4 {
            return Err("quote".into());
        }
        limbs[index] = digit;
    }
    Ok(U256::from_limbs(limbs))
}
