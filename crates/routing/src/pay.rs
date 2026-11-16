//! Chord pay on stack integers when the factors fit in 256 bits.
//!
//! `output * taken / width` is truncating division. For nonnegative integers
//! that is the floor. A product that fits in 512 bits and a quotient that fits
//! in 256 bits uses the same division as `U256`. Anything wider, or any
//! negative factor, stays on `BigInt`.

use alloy_primitives::{Uint, U256};
use num_bigint::{BigInt, Sign};
use num_traits::Zero;
use std::cmp::Ordering;

use crate::SlopeSegment;

type U512 = Uint<512, 8>;

pub fn chord_pay(output: &BigInt, taken: &BigInt, width: &BigInt) -> BigInt {
    pay_wide(output, taken, width).unwrap_or_else(|| (output * taken) / width)
}

/// Compare `left_out * left_width` with `right_out * right_width`.
pub(crate) fn cross_cmp(
    left_out: &BigInt,
    left_width: &BigInt,
    right_out: &BigInt,
    right_width: &BigInt,
) -> Ordering {
    if let (Some(left_out), Some(left_width), Some(right_out), Some(right_width)) = (
        u256_of(left_out),
        u256_of(left_width),
        u256_of(right_out),
        u256_of(right_width),
    ) {
        return wide_mul(left_out, left_width).cmp(&wide_mul(right_out, right_width));
    }
    (left_out * left_width).cmp(&(right_out * right_width))
}

pub(crate) fn quote_segments(segments: &[SlopeSegment], input: &BigInt) -> BigInt {
    quote_marginals(
        segments
            .iter()
            .map(|segment| (&segment.width, &segment.output)),
        input,
    )
}

/// `(width, output)` pairs, in spend order. Same division as [`crate::quote`].
pub fn quote_marginals<'a>(
    marginals: impl Iterator<Item = (&'a BigInt, &'a BigInt)> + Clone,
    input: &BigInt,
) -> BigInt {
    if input.sign() != Sign::Plus {
        return BigInt::from(0);
    }
    if let Some(paid) = quote_u128(marginals.clone(), input) {
        return paid;
    }
    if let Some(paid) = quote_wide(marginals.clone(), input) {
        return paid;
    }
    let mut left = input.clone();
    let mut paid = BigInt::from(0);
    for (width, output) in marginals {
        if left.is_zero() || width.is_zero() {
            break;
        }
        let taken = if &left < width {
            left.clone()
        } else {
            width.clone()
        };
        paid += chord_pay(output, &taken, width);
        left -= &taken;
    }
    paid
}

fn quote_u128<'a>(
    marginals: impl Iterator<Item = (&'a BigInt, &'a BigInt)>,
    input: &BigInt,
) -> Option<BigInt> {
    let mut left = u128_of(input)?;
    let mut paid = 0u128;
    for (width, output) in marginals {
        if left == 0 {
            break;
        }
        let width = u128_of(width)?;
        if width == 0 {
            break;
        }
        let output = u128_of(output)?;
        let taken = if left < width { left } else { width };
        let product = output.checked_mul(taken)?;
        paid = paid.checked_add(product / width)?;
        left -= taken;
    }
    Some(BigInt::from(paid))
}

pub(crate) fn u128_of(value: &BigInt) -> Option<u128> {
    if value.sign() == Sign::Minus {
        return None;
    }
    let mut limbs = [0u64; 2];
    for (index, digit) in value.magnitude().iter_u64_digits().enumerate() {
        if index == 2 {
            return None;
        }
        limbs[index] = digit;
    }
    Some(u128::from(limbs[0]) | (u128::from(limbs[1]) << 64))
}

fn quote_wide<'a>(
    marginals: impl Iterator<Item = (&'a BigInt, &'a BigInt)>,
    input: &BigInt,
) -> Option<BigInt> {
    let mut left = u256_of(input)?;
    let mut paid = U256::ZERO;
    for (width, output) in marginals {
        if left.is_zero() {
            break;
        }
        let width = u256_of(width)?;
        if width.is_zero() {
            break;
        }
        let output = u256_of(output)?;
        let taken = if left < width { left } else { width };
        paid = paid.checked_add(mul_div_u256(output, taken, width)?)?;
        left -= taken;
    }
    Some(bigint_of(paid))
}

fn pay_wide(output: &BigInt, taken: &BigInt, width: &BigInt) -> Option<BigInt> {
    Some(bigint_of(mul_div_u256(
        u256_of(output)?,
        u256_of(taken)?,
        u256_of(width)?,
    )?))
}

pub(crate) fn wide_mul(left: U256, right: U256) -> U512 {
    left.widening_mul(right)
}

pub(crate) fn mul_div_u256(output: U256, taken: U256, width: U256) -> Option<U256> {
    if width.is_zero() {
        return None;
    }
    let (quot, _) = output.widening_mul(taken).div_rem(u512_of(width));
    narrow(quot)
}

pub(crate) fn u256_of(value: &BigInt) -> Option<U256> {
    if value.sign() == Sign::Minus {
        return None;
    }
    let mut limbs = [0u64; 4];
    for (index, digit) in value.magnitude().iter_u64_digits().enumerate() {
        if index == 4 {
            return None;
        }
        limbs[index] = digit;
    }
    Some(U256::from_limbs(limbs))
}

pub(crate) fn bigint_of(value: U256) -> BigInt {
    match u128::try_from(value) {
        Ok(small) => BigInt::from(small),
        Err(_) => BigInt::from_bytes_le(Sign::Plus, &value.to_le_bytes::<32>()),
    }
}

fn u512_of(value: U256) -> U512 {
    let limbs = value.as_limbs();
    U512::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3], 0, 0, 0, 0])
}

fn narrow(value: U512) -> Option<U256> {
    let limbs = value.as_limbs();
    if limbs[4] | limbs[5] | limbs[6] | limbs[7] != 0 {
        return None;
    }
    Some(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pay(output: i64, taken: i64, width: i64) -> BigInt {
        chord_pay(
            &BigInt::from(output),
            &BigInt::from(taken),
            &BigInt::from(width),
        )
    }

    #[test]
    fn wide_pay_matches_truncating_division() {
        assert_eq!(pay(10, 3, 4), BigInt::from(7));
        assert_eq!(pay(10, 4, 4), BigInt::from(10));
        let output = BigInt::from(10).pow(21);
        let taken = BigInt::from(10).pow(21);
        let width = BigInt::from(10).pow(18);
        let expected = (&output * &taken) / &width;
        assert_eq!(chord_pay(&output, &taken, &width), expected);
    }

    #[test]
    fn a_quotient_past_256_bits_stays_on_bigint() {
        let output = BigInt::from(10).pow(60);
        let taken = BigInt::from(10).pow(40);
        let width = BigInt::from(10).pow(20);
        let expected = (&output * &taken) / &width;
        assert_eq!(chord_pay(&output, &taken, &width), expected);
    }

    #[test]
    fn cross_products_keep_order() {
        let left_out = BigInt::from(10).pow(22);
        let right_out = &left_out - 1;
        let width = BigInt::from(3);
        assert_eq!(
            cross_cmp(&left_out, &width, &right_out, &width),
            Ordering::Greater
        );
        assert_eq!(
            cross_cmp(&left_out, &width, &left_out, &width),
            Ordering::Equal
        );
    }
}
