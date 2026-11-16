//! Constant-product and hyperbola quotes.

use super::share::*;
use alloy_primitives::{Uint, U256};
use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_traits::{Signed, Zero};

type U512 = Uint<512, 8>;

pub(crate) fn cp_num(y: &BigInt, k: &BigInt, dx: &BigInt) -> BigInt {
    y * k * dx
}

pub(crate) fn cp_den(x: &BigInt, k: &BigInt, dx: &BigInt) -> BigInt {
    x + k * dx
}

#[allow(non_snake_case)]
pub(crate) fn hyp_D(x1: &BigInt, y1: &BigInt, x2: &BigInt, y2: &BigInt) -> BigInt {
    x2 * y1 - x1 * y2
}

#[allow(non_snake_case)]
pub(crate) fn hyp_N(x1: &BigInt, y1: &BigInt, x2: &BigInt, y2: &BigInt) -> BigInt {
    x1 * x2 * (y2 - y1)
}

#[allow(non_snake_case)]
pub(crate) fn hyp_A(x1: &BigInt, y1: &BigInt, x2: &BigInt, y2: &BigInt) -> BigInt {
    y1 * y2 * (x2 - x1)
}

#[allow(non_snake_case)]
pub(crate) fn hyp_Q(x1: &BigInt, y1: &BigInt, x2: &BigInt, y2: &BigInt, x: &BigInt) -> BigInt {
    hyp_N(x1, y1, x2, y2) + hyp_D(x1, y1, x2, y2) * x
}

#[allow(non_snake_case)]
pub(crate) fn hyp_P(x1: &BigInt, y1: &BigInt, x2: &BigInt, y2: &BigInt, x: &BigInt) -> BigInt {
    x * hyp_A(x1, y1, x2, y2)
}

pub(crate) fn mul_div_up(numer_a: &BigInt, numer_b: &BigInt, denom: &BigInt) -> Option<BigInt> {
    if denom <= &BigInt::zero() {
        return None;
    }
    Some((numer_a * numer_b).div_ceil(denom))
}

pub(crate) fn div_up(value: &BigInt, denom: &BigInt) -> Option<BigInt> {
    if denom <= &BigInt::zero() {
        return None;
    }
    Some(value.div_ceil(denom))
}

pub(crate) fn product_out(
    reserve_in: &BigInt,
    reserve_out: &BigInt,
    amount: &BigInt,
    fee: &BigInt,
    fee_denom: &BigInt,
) -> Option<BigInt> {
    if reserve_in <= &BigInt::zero()
        || reserve_out <= &BigInt::zero()
        || amount < &BigInt::zero()
        || fee < &BigInt::zero()
        || fee_denom <= &BigInt::zero()
        || fee > fee_denom
    {
        return None;
    }
    if let Some(out) = product_wide(reserve_in, reserve_out, amount, fee, fee_denom) {
        return Some(out);
    }
    product_bigint(reserve_in, reserve_out, amount, fee, fee_denom)
}

fn product_bigint(
    reserve_in: &BigInt,
    reserve_out: &BigInt,
    amount: &BigInt,
    fee: &BigInt,
    fee_denom: &BigInt,
) -> Option<BigInt> {
    // Fee is withheld from the input. This is not the k-multiplier model.
    let after = floor_div(&(amount * (fee_denom - fee)), fee_denom);
    Some(floor_div(&(reserve_out * &after), &(reserve_in + &after)))
}

/// Truncating product quote when every value fits in 256 bits.
fn product_wide(
    reserve_in: &BigInt,
    reserve_out: &BigInt,
    amount: &BigInt,
    fee: &BigInt,
    fee_denom: &BigInt,
) -> Option<BigInt> {
    let reserve_in = u256_nonneg(reserve_in)?;
    let reserve_out = u256_nonneg(reserve_out)?;
    let amount = u256_nonneg(amount)?;
    let fee = u256_nonneg(fee)?;
    let fee_denom = u256_nonneg(fee_denom)?;
    let span = fee_denom.checked_sub(fee)?;
    let after = mul_div_floor(amount, span, fee_denom)?;
    let den = reserve_in.checked_add(after)?;
    let out = mul_div_floor(reserve_out, after, den)?;
    Some(bigint_nonneg(out))
}

fn u256_nonneg(value: &BigInt) -> Option<U256> {
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

fn bigint_nonneg(value: U256) -> BigInt {
    match u128::try_from(value) {
        Ok(small) => BigInt::from(small),
        Err(_) => BigInt::from_bytes_le(Sign::Plus, &value.to_le_bytes::<32>()),
    }
}

fn mul_div_floor(numer_a: U256, numer_b: U256, den: U256) -> Option<U256> {
    if den.is_zero() {
        return None;
    }
    let product: U512 = numer_a.widening_mul(numer_b);
    let den = {
        let limbs = den.as_limbs();
        U512::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3], 0, 0, 0, 0])
    };
    let (quot, _) = product.div_rem(den);
    let limbs = quot.as_limbs();
    if limbs[4] != 0 || limbs[5] != 0 || limbs[6] != 0 || limbs[7] != 0 {
        return None;
    }
    Some(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

pub(crate) fn samples_ok(x1: &BigInt, y1: &BigInt, x2: &BigInt, y2: &BigInt) -> bool {
    x1.is_positive()
        && x2 > x1
        && y1.is_positive()
        && y2 >= y1
        && hyp_D(x1, y1, x2, y2).is_positive()
}

pub(crate) fn hyperbola_out(
    x1: &BigInt,
    y1: &BigInt,
    x2: &BigInt,
    y2: &BigInt,
    amount: &BigInt,
) -> Option<BigInt> {
    if amount < &BigInt::zero() || !samples_ok(x1, y1, x2, y2) {
        return None;
    }
    let den = hyp_Q(x1, y1, x2, y2, amount);
    if den <= BigInt::zero() {
        return None;
    }
    Some(floor_div(&hyp_P(x1, y1, x2, y2, amount), &den))
}

pub(crate) fn hyperbola_in(
    x1: &BigInt,
    y1: &BigInt,
    x2: &BigInt,
    y2: &BigInt,
    output: &BigInt,
) -> Option<BigInt> {
    if output < &BigInt::zero() || !samples_ok(x1, y1, x2, y2) {
        return None;
    }
    let den = hyp_A(x1, y1, x2, y2) - output * hyp_D(x1, y1, x2, y2);
    if den <= BigInt::zero() {
        return None;
    }
    Some(floor_div(&(output * hyp_N(x1, y1, x2, y2)), &den))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_product_matches_truncating_division() {
        let small = [
            (1000i64, 2000i64, 10i64, 3i64, 1000i64),
            (
                1_000_000_000_000_000_000,
                2_000_000_000_000_000_000,
                10_000_000_000_000_000,
                3000,
                1_000_000,
            ),
        ];
        for (reserve_in, reserve_out, amount, fee, denom) in small {
            let reserve_in = BigInt::from(reserve_in);
            let reserve_out = BigInt::from(reserve_out);
            let amount = BigInt::from(amount);
            let fee = BigInt::from(fee);
            let denom = BigInt::from(denom);
            let wide = product_wide(&reserve_in, &reserve_out, &amount, &fee, &denom);
            let slow = product_bigint(&reserve_in, &reserve_out, &amount, &fee, &denom);
            assert_eq!(wide, slow);
            assert!(wide.is_some());
        }
        let reserve_in = BigInt::from(1000u64) * BigInt::from(10).pow(18);
        let reserve_out = BigInt::from(2000u64) * BigInt::from(10).pow(18);
        let amount = BigInt::from(10).pow(16);
        let fee = BigInt::from(3000);
        let denom = BigInt::from(1_000_000);
        assert_eq!(
            product_out(&reserve_in, &reserve_out, &amount, &fee, &denom),
            product_bigint(&reserve_in, &reserve_out, &amount, &fee, &denom)
        );
        let wide_reserve = BigInt::from(1) << 300;
        assert!(product_wide(&wide_reserve, &reserve_out, &amount, &fee, &denom).is_none());
        assert_eq!(
            product_out(&wide_reserve, &reserve_out, &amount, &fee, &denom),
            product_bigint(&wide_reserve, &reserve_out, &amount, &fee, &denom)
        );
    }
}
