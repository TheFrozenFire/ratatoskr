//! Curve crypto and flat-pair quotes.

use super::share::*;
use num_bigint::BigInt;
use num_traits::{One, Signed, Zero};

pub(crate) fn g1(gamma: &BigInt, k0: &BigInt) -> BigInt {
    let base = gamma + &*WAD;
    if &base > k0 {
        &base - k0 + 1
    } else {
        k0 - &base + 1
    }
}

pub(crate) fn crypto_y2(
    ann: &BigInt,
    gamma: &BigInt,
    x: &[BigInt],
    d: &BigInt,
    i: i64,
) -> Option<BigInt> {
    if !ann.is_positive()
        || !gamma.is_positive()
        || !d.is_positive()
        || (i != 0 && i != 1)
        || x.len() < 2
    {
        return None;
    }
    let x_j = &x[(1 - i) as usize];
    if !x_j.is_positive() {
        return None;
    }
    let mut y = floor_div(&(d * d), &(x_j * 4));
    let k0_i = floor_div(&(&*WAD * 2 * x_j), d);
    let ten14 = pow10(14);
    let limit = floor_div(x_j, &ten14)
        .max(floor_div(d, &ten14))
        .max(BigInt::from(100));
    for _ in 0..255 {
        if !y.is_positive() {
            return None;
        }
        let previous = y.clone();
        let k0 = floor_div(&(&k0_i * &y * 2), d);
        if !k0.is_positive() {
            return None;
        }
        let g = g1(gamma, &k0);
        if !g.is_positive() {
            return None;
        }
        let mut mul1 = floor_div(&(&*WAD * d), gamma);
        mul1 = floor_div(&(&mul1 * &g), gamma);
        mul1 = floor_div(&(&mul1 * &g * &*A_MULTIPLIER), ann);
        let mul2 = &*WAD + floor_div(&(&(BigInt::from(2) * &*WAD) * &k0), &g);
        let mut yfprime = &*WAD * &y + (x_j + &y) * &mul2 + &mul1;
        let dyfprime = d * &mul2;
        if yfprime < dyfprime {
            y = floor_div(&previous, &BigInt::from(2));
            continue;
        }
        yfprime -= &dyfprime;
        let fprime = floor_div(&yfprime, &y);
        if !fprime.is_positive() {
            return None;
        }
        let y_minus = floor_div(&mul1, &fprime) + floor_div(&(&*WAD * (x_j + &y)), &fprime);
        let y_plus = floor_div(&(&yfprime + &*WAD * d), &fprime)
            + floor_div(&(floor_div(&mul1, &fprime) * &*WAD), &k0);
        y = if y_plus < y_minus {
            floor_div(&previous, &BigInt::from(2))
        } else {
            y_plus - y_minus
        };
        let tol = limit.clone().max(floor_div(&y, &ten14));
        if (&y - &previous).abs() < tol {
            return Some(y);
        }
    }
    None
}

pub(crate) fn crypto_scale(
    ann: &BigInt,
    gamma: &BigInt,
    d: &BigInt,
    balance0: &BigInt,
    balance1: &BigInt,
    prec0: &BigInt,
    prec1: &BigInt,
) -> Option<BigInt> {
    let y = crypto_y2(ann, gamma, &[balance0 * prec0, BigInt::zero()], d, 1)?;
    let den = balance1 * prec1;
    if !den.is_positive() {
        return None;
    }
    Some(floor_div(&(&y * &*WAD), &den))
}

pub(crate) fn flat_y(amp: &BigInt, xp: &[BigInt], d: &BigInt, i: i64) -> Option<BigInt> {
    if !amp.is_positive() || !d.is_positive() || (i != 0 && i != 1) || xp.len() != 2 {
        return None;
    }
    let other = &xp[(1 - i) as usize];
    if !other.is_positive() {
        return None;
    }
    let n = BigInt::from(2);
    let ann = amp * &n;
    let mut c = floor_div(&(d * d), &(other * &n));
    c = floor_div(&(&c * d * &*A_MULTIPLIER), &(&ann * &n));
    let b = other + floor_div(&(d * &*A_MULTIPLIER), &ann);
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

pub(crate) fn flat_scale(
    amp: &BigInt,
    d: &BigInt,
    balance0: &BigInt,
    balance1: &BigInt,
    prec0: &BigInt,
    prec1: &BigInt,
) -> Option<BigInt> {
    let y = flat_y(amp, &[balance0 * prec0, BigInt::zero()], d, 1)?;
    let den = balance1 * prec1;
    if !den.is_positive() {
        return None;
    }
    Some(floor_div(&(&y * &*WAD), &den))
}

pub(crate) fn flat_fee(
    xp: &[BigInt],
    mid_fee: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
) -> BigInt {
    let total = &xp[0] + &xp[1];
    if !total.is_positive() || !fee_gamma.is_positive() {
        return mid_fee.clone();
    }
    let mut k = floor_div(&(&*WAD * 4 * &xp[0]), &total);
    k = floor_div(&(&k * &xp[1]), &total);
    let den = floor_div(&(fee_gamma * &k), &*WAD) + &*WAD - &k;
    if !den.is_positive() {
        return mid_fee.clone();
    }
    let reduction = floor_div(&(fee_gamma * &k), &den);
    floor_div(
        &(mid_fee * &reduction + out_fee * (&*WAD - &reduction)),
        &*WAD,
    )
}

pub(crate) fn crypto_fee2(
    xp: &[BigInt],
    mid_fee: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
) -> BigInt {
    let total = &xp[0] + &xp[1];
    if !total.is_positive() || !fee_gamma.is_positive() {
        return mid_fee.clone();
    }
    let mut k = floor_div(&(&*WAD * 4 * &xp[0]), &total);
    k = floor_div(&(&k * &xp[1]), &total);
    let den = fee_gamma + &*WAD - &k;
    if !den.is_positive() {
        return mid_fee.clone();
    }
    let reduction = floor_div(&(fee_gamma * &*WAD), &den);
    floor_div(
        &(mid_fee * &reduction + out_fee * (&*WAD - &reduction)),
        &*WAD,
    )
}

pub(crate) fn scaled_pair(
    balances: &[BigInt],
    precs: &[BigInt],
    scale: &BigInt,
    i: i64,
    dx: &BigInt,
) -> Option<Vec<BigInt>> {
    if balances.len() < 2 || precs.len() < 2 {
        return None;
    }
    let grown0 = &balances[0] + if i == 0 { dx.clone() } else { BigInt::zero() };
    let grown1 = &balances[1] + if i == 1 { dx.clone() } else { BigInt::zero() };
    let xp = vec![
        &grown0 * &precs[0],
        floor_div(&(&grown1 * scale), &*WAD) * &precs[1],
    ];
    if !xp[0].is_positive() || !xp[1].is_positive() {
        return None;
    }
    Some(xp)
}

pub(crate) fn finish_pair(
    mut xp: Vec<BigInt>,
    y: BigInt,
    j: i64,
    scale: &BigInt,
    precs: &[BigInt],
    mid_fee: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
    flat: bool,
) -> Option<BigInt> {
    if xp[j as usize] <= &y + 1 {
        return None;
    }
    let mut dy = &xp[j as usize] - &y - 1;
    xp[j as usize] = y;
    if j > 0 {
        dy = floor_div(&(&dy * &*WAD), scale);
    }
    dy = floor_div(&dy, &precs[j as usize]);
    let fee = if flat {
        flat_fee(&xp, mid_fee, out_fee, fee_gamma)
    } else {
        crypto_fee2(&xp, mid_fee, out_fee, fee_gamma)
    };
    let taken = floor_div(&(&fee * &dy), &FEE_1E10);
    Some(dy - taken)
}

pub(crate) fn flat_pair_out(
    balances: &[BigInt],
    precs: &[BigInt],
    scale: &BigInt,
    amp: &BigInt,
    d: &BigInt,
    i: i64,
    j: i64,
    dx: &BigInt,
    mid_fee: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
    flat_fee_on: bool,
) -> Option<BigInt> {
    if !scale.is_positive()
        || dx.is_negative()
        || (i != 0 && i != 1)
        || (j != 0 && j != 1)
        || i == j
    {
        return None;
    }
    let xp = scaled_pair(balances, precs, scale, i, dx)?;
    let y = flat_y(amp, &xp, d, j)?;
    finish_pair(
        xp,
        y,
        j,
        scale,
        precs,
        mid_fee,
        out_fee,
        fee_gamma,
        flat_fee_on,
    )
}

pub fn crypto2_out(
    balances: &[BigInt],
    precs: &[BigInt],
    scale: &BigInt,
    ann: &BigInt,
    gamma: &BigInt,
    d: &BigInt,
    i: i64,
    j: i64,
    dx: &BigInt,
    mid_fee: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
) -> Option<BigInt> {
    if !scale.is_positive()
        || dx.is_negative()
        || (i != 0 && i != 1)
        || (j != 0 && j != 1)
        || i == j
    {
        return None;
    }
    let xp = scaled_pair(balances, precs, scale, i, dx)?;
    let y = crypto_y2(ann, gamma, &xp, d, j)?;
    finish_pair(xp, y, j, scale, precs, mid_fee, out_fee, fee_gamma, false)
}

pub(crate) fn crypto_y3(
    ann: &BigInt,
    gamma: &BigInt,
    x: &[BigInt],
    d: &BigInt,
    i: i64,
) -> Option<BigInt> {
    if !ann.is_positive()
        || !gamma.is_positive()
        || !d.is_positive()
        || !(0..=2).contains(&i)
        || x.len() != 3
    {
        return None;
    }
    let mut y = floor_div(d, &BigInt::from(3));
    let mut k0_i = WAD.clone();
    let mut s_i = BigInt::zero();
    let mut ordered = x.to_vec();
    ordered[i as usize] = BigInt::zero();
    if ordered[0] < ordered[1] {
        ordered.swap(0, 1);
    }
    if ordered[0] < ordered[2] {
        ordered.swap(0, 2);
    }
    if ordered[1] < ordered[2] {
        ordered.swap(1, 2);
    }
    let ten14 = pow10(14);
    let limit = floor_div(&ordered[0], &ten14)
        .max(floor_div(d, &ten14))
        .max(BigInt::from(100));
    for step in 2..4 {
        let known = ordered[3 - step].clone();
        if !known.is_positive() {
            return None;
        }
        y = floor_div(&(&y * d), &(&known * 3));
        s_i += &known;
    }
    for val in &ordered[..2] {
        k0_i = floor_div(&(&k0_i * val * 3), d);
    }
    for _ in 0..255 {
        if !y.is_positive() || !k0_i.is_positive() {
            return None;
        }
        let previous = y.clone();
        let k0 = floor_div(&(&k0_i * &y * 3), d);
        if !k0.is_positive() {
            return None;
        }
        let g = g1(gamma, &k0);
        if !g.is_positive() {
            return None;
        }
        let mut mul1 = floor_div(&(&*WAD * d), gamma);
        mul1 = floor_div(&(&mul1 * &g), gamma);
        mul1 = floor_div(&(&mul1 * &g * &*A_MULTIPLIER), ann);
        let mul2 = &*WAD + floor_div(&(&(BigInt::from(2) * &*WAD) * &k0), &g);
        let mut yfprime = &*WAD * &y + (&s_i + &y) * &mul2 + &mul1;
        let dyfprime = d * &mul2;
        if yfprime < dyfprime {
            y = floor_div(&previous, &BigInt::from(2));
            continue;
        }
        yfprime -= &dyfprime;
        let fprime = floor_div(&yfprime, &y);
        if !fprime.is_positive() {
            return None;
        }
        let mut y_minus = floor_div(&mul1, &fprime);
        let y_plus =
            floor_div(&(&yfprime + &*WAD * d), &fprime) + floor_div(&(&y_minus * &*WAD), &k0);
        y_minus += floor_div(&(&*WAD * (&s_i + &y)), &fprime);
        y = if y_plus < y_minus {
            floor_div(&previous, &BigInt::from(2))
        } else {
            y_plus - y_minus
        };
        let tol = limit.clone().max(floor_div(&y, &ten14));
        if (&y - &previous).abs() < tol {
            return Some(y);
        }
    }
    None
}

pub(crate) fn crypto_fee3(
    xp: &[BigInt],
    mid_fee: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
) -> BigInt {
    let total = &xp[0] + &xp[1] + &xp[2];
    if !total.is_positive() {
        return BigInt::zero();
    }
    let mut k = floor_div(&(&*WAD * 3 * &xp[0]), &total);
    k = floor_div(&(&k * 3 * &xp[1]), &total);
    k = floor_div(&(&k * 3 * &xp[2]), &total);
    if fee_gamma.is_positive() {
        let den = fee_gamma + &*WAD - &k;
        if !den.is_positive() {
            return mid_fee.clone();
        }
        k = floor_div(&(fee_gamma * &*WAD), &den);
    }
    floor_div(&(mid_fee * &k + out_fee * (&*WAD - &k)), &*WAD)
}

pub fn crypto3_out(
    balances: &[BigInt],
    precs: &[BigInt],
    scales: &[BigInt],
    ann: &BigInt,
    gamma: &BigInt,
    d: &BigInt,
    i: i64,
    j: i64,
    dx: &BigInt,
    mid_fee: &BigInt,
    out_fee: &BigInt,
    fee_gamma: &BigInt,
) -> Option<BigInt> {
    if dx.is_negative()
        || !(0..=2).contains(&i)
        || !(0..=2).contains(&j)
        || i == j
        || balances.len() < 3
        || precs.len() < 3
        || scales.len() < 2
        || !scales[0].is_positive()
        || !scales[1].is_positive()
    {
        return None;
    }
    let mut grown = balances[..3].to_vec();
    grown[i as usize] += dx;
    let mut xp = vec![
        &grown[0] * &precs[0],
        floor_div(&(&grown[1] * &scales[0]), &*WAD) * &precs[1],
        floor_div(&(&grown[2] * &scales[1]), &*WAD) * &precs[2],
    ];
    if xp.iter().any(|value| !value.is_positive()) {
        return None;
    }
    let y = crypto_y3(ann, gamma, &xp, d, j)?;
    if xp[j as usize] <= &y + 1 {
        return None;
    }
    let mut dy = &xp[j as usize] - &y - 1;
    xp[j as usize] = y;
    if j > 0 {
        dy = floor_div(&(&dy * &*WAD), &scales[(j - 1) as usize]);
    }
    dy = floor_div(&dy, &precs[j as usize]);
    let fee = crypto_fee3(&xp, mid_fee, out_fee, fee_gamma);
    let taken = floor_div(&(&fee * &dy), &FEE_1E10);
    Some(dy - taken)
}
