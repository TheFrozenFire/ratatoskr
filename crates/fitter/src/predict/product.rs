//! Constant-product and hyperbola quotes.

use super::share::*;
use num_bigint::BigInt;
use num_integer::Integer;
use num_traits::{Signed, Zero};

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
    // Fee is withheld from the input. This is not the k-multiplier model.
    let after = floor_div(&(amount * (fee_denom - fee)), fee_denom);
    Some(floor_div(&(reserve_out * &after), &(reserve_in + &after)))
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
