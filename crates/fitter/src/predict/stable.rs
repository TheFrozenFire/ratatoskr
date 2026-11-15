//! Stableswap invariant quotes.

use super::share::*;
use num_bigint::BigInt;
use num_traits::{One, Signed, ToPrimitive, Zero};

pub(crate) fn stable_d(xp: &[BigInt], amp: &BigInt) -> Option<BigInt> {
    let n_len = xp.len();
    if n_len < 2 || !amp.is_positive() {
        return None;
    }
    let mut total = BigInt::zero();
    for value in xp {
        if !value.is_positive() {
            return None;
        }
        total += value;
    }
    if !total.is_positive() {
        return None;
    }
    let n = BigInt::from(n_len);
    let ann = amp * &n;
    let mut d = total.clone();
    // Newton stops at 255.
    for _ in 0..255 {
        let mut d_p = d.clone();
        for value in xp {
            d_p = floor_div(&(&d_p * &d), &(value * &n));
        }
        let previous = d.clone();
        let numerator = (&ann * &total + &d_p * &n) * &d;
        let denominator = (&ann - BigInt::one()) * &d + (&n + BigInt::one()) * &d_p;
        if !denominator.is_positive() {
            return None;
        }
        d = floor_div(&numerator, &denominator);
        if (&d - &previous).abs() <= BigInt::one() {
            return Some(d);
        }
    }
    None
}

pub(crate) fn stable_y(
    i: i64,
    j: i64,
    x: &BigInt,
    xp: &[BigInt],
    amp: &BigInt,
    d: &BigInt,
) -> Option<BigInt> {
    let n_len = xp.len() as i64;
    if i == j
        || i < 0
        || j < 0
        || i >= n_len
        || j >= n_len
        || !amp.is_positive()
        || !d.is_positive()
        || !x.is_positive()
    {
        return None;
    }
    let n = BigInt::from(n_len);
    let ann = amp * &n;
    let mut c = d.clone();
    let mut s = BigInt::zero();
    for (k, balance) in xp.iter().enumerate() {
        if k as i64 == j {
            continue;
        }
        let x_k = if k as i64 == i {
            x.clone()
        } else {
            balance.clone()
        };
        if !x_k.is_positive() {
            return None;
        }
        s += &x_k;
        c = floor_div(&(&c * d), &(&x_k * &n));
    }
    c = floor_div(&(&c * d), &(&ann * &n));
    let b = s + floor_div(d, &ann);
    let mut y = d.clone();
    for _ in 0..255 {
        let previous = y.clone();
        let den = &y * BigInt::from(2) + &b - d;
        if !den.is_positive() {
            return None;
        }
        y = floor_div(&(&y * &y + &c), &den);
        if (&y - &previous).abs() <= BigInt::one() {
            return Some(y);
        }
    }
    None
}

pub(crate) fn dynamic_fee(
    xpi: &BigInt,
    xpj: &BigInt,
    fee: &BigInt,
    multiplier: &BigInt,
    fee_denom: &BigInt,
) -> BigInt {
    if multiplier <= fee_denom || !fee.is_positive() || xpi.is_negative() || xpj.is_negative() {
        return fee.clone();
    }
    let total = xpi + xpj;
    if !total.is_positive() {
        return fee.clone();
    }
    let squared = &total * &total;
    let product = (multiplier - fee_denom) * 4 * xpi * xpj;
    let denom = floor_div(&product, &squared) + fee_denom;
    if !denom.is_positive() {
        return fee.clone();
    }
    floor_div(&(multiplier * fee), &denom)
}

pub(crate) fn stable_swap_out(
    xp: &[BigInt],
    amp: BigInt,
    i: BigInt,
    j: BigInt,
    dx: BigInt,
    fee: BigInt,
    fee_denom: BigInt,
    fee_on_input: bool,
    offpeg: BigInt,
) -> Option<BigInt> {
    let (Some(i), Some(j)) = (i.to_i64(), j.to_i64()) else {
        return None;
    };
    if dx.is_negative() || fee.is_negative() || !fee_denom.is_positive() || fee > fee_denom {
        return None;
    }
    if i < 0 || j < 0 || i as usize >= xp.len() || j as usize >= xp.len() {
        return None;
    }
    let d = stable_d(xp, &amp)?;
    let mut paid = dx.clone();
    if fee_on_input && !fee.is_zero() {
        paid = floor_div(&(&dx * (&fee_denom - &fee)), &fee_denom);
    }
    let y = stable_y(i, j, &(&xp[i as usize] + &paid), xp, &amp, &d)?;
    if xp[j as usize] <= &y + 1 {
        return None;
    }
    let mut dy = &xp[j as usize] - &y - 1;
    if !fee_on_input && !fee.is_zero() {
        let mut used = fee.clone();
        if offpeg > fee_denom {
            used = dynamic_fee(
                &floor_div(
                    &(&xp[i as usize] + &xp[i as usize] + &paid),
                    &BigInt::from(2),
                ),
                &floor_div(&(&xp[j as usize] + &y), &BigInt::from(2)),
                &fee,
                &offpeg,
                &fee_denom,
            );
        }
        let taken = floor_div(&(&dy * &used), &fee_denom);
        if taken >= dy {
            return Some(BigInt::zero());
        }
        dy -= taken;
    }
    Some(dy)
}
