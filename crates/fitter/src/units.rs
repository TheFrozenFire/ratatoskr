//! Integer and decimal helpers shared by evaluation and selection.

use bigdecimal::{Context, RoundingMode};
use num_bigint::BigInt;
use num_integer::Integer;
use num_rational::BigRational;
use num_traits::{One, Zero};
use std::num::NonZeroU64;

use crate::formula::Formula;
use crate::predict::{A_MULTIPLIER, DUST, FEE_1E10, PRECISIONS, WAD};

pub(crate) type Scored = (i64, Formula);

pub(crate) const PREC_40: NonZeroU64 = NonZeroU64::new(40).unwrap();

// Holdout fractions are exactly these strings.
pub(crate) const HOLD_FRACTIONS: [&str; 2] = ["0.25", "0.8"];

pub(crate) fn wad() -> BigInt {
    (*WAD).clone()
}

pub(crate) fn fee_denom() -> BigInt {
    (*FEE_1E10).clone()
}

pub(crate) fn a_multiplier() -> BigInt {
    (*A_MULTIPLIER).clone()
}

pub(crate) fn dust() -> BigInt {
    (*DUST).clone()
}

pub(crate) fn precisions() -> Vec<BigInt> {
    PRECISIONS.iter().cloned().collect()
}

pub(crate) fn bi(value: i64) -> BigInt {
    BigInt::from(value)
}

pub(crate) fn pow10(exp: u32) -> BigInt {
    let exp = exp.min(4_096);
    let mut acc = BigInt::one();
    let mut base = BigInt::from(10);
    let mut left = exp;
    while left > 0 {
        if left & 1 == 1 {
            acc *= &base;
        }
        left >>= 1;
        if left > 0 {
            base = &base * &base;
        }
    }
    acc
}

pub(crate) fn rat_i(value: i64) -> BigRational {
    BigRational::from_integer(bi(value))
}

pub(crate) fn fd(numer: &BigInt, denom: &BigInt) -> BigInt {
    if denom.is_zero() {
        BigInt::zero()
    } else {
        numer.div_floor(denom)
    }
}

pub(crate) fn trunc_rat(value: &BigRational) -> BigInt {
    value.numer() / value.denom()
}

pub(crate) fn ctx_at(precision: NonZeroU64) -> Context {
    Context::new(precision, RoundingMode::HalfEven)
}

pub(crate) fn dec_ctx() -> Context {
    ctx_at(PREC_40)
}

pub(crate) fn max_bi(left: BigInt, right: BigInt) -> BigInt {
    if left > right {
        left
    } else {
        right
    }
}

pub(crate) fn min_bi(left: BigInt, right: BigInt) -> BigInt {
    if left < right {
        left
    } else {
        right
    }
}

pub(crate) fn ternary_points(left: &BigInt, right: &BigInt) -> (BigInt, BigInt) {
    let span = right - left;
    let third = fd(&span, &bi(3));
    (left + &third, right - third)
}

pub(crate) fn left_strict(left: Option<i64>, right: Option<i64>) -> bool {
    right.is_none() || left.is_some_and(|left| right.is_none_or(|right| left < right))
}
