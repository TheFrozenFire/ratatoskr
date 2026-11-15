//! Balancer weighted quotes.

use super::product::product_out;
use super::share::*;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed};

pub(crate) fn weighted_out(
    balance_in: &BigInt,
    balance_out: &BigInt,
    weight_in: &BigInt,
    weight_out: &BigInt,
    amount: &BigInt,
    fee: &BigInt,
) -> Option<BigInt> {
    if !weight_in.is_positive() || !weight_out.is_positive() || fee.is_negative() || fee > &*WAD {
        return None;
    }
    if weight_in == weight_out {
        return product_out(balance_in, balance_out, amount, fee, &*WAD);
    }
    if !balance_in.is_positive() || !balance_out.is_positive() || amount.is_negative() {
        return None;
    }
    // Fee is withheld from the input. The power uses an 80-digit floor.
    let after = floor_div(&(amount * (&*WAD - fee)), &*WAD);
    let den = balance_in + &after;
    if !den.is_positive() {
        return None;
    }
    let base = floor80_ratio(&BigRational::new(balance_in.clone(), den));
    let exponent = floor80_ratio(&BigRational::new(weight_in.clone(), weight_out.clone()));
    let kept = decimal_pow(&base, &exponent);
    if kept > BigRational::one() {
        return None;
    }
    let factor = floor80_ratio(&(&BigRational::one() - &kept));
    let out = floor80_ratio(&(BigRational::from_integer(balance_out.clone()) * &factor));
    if out.numer().is_negative() {
        return None;
    }
    Some(floor_rat(&out))
}
