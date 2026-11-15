//! Chord, cubic, and residual quotes on a knot list.

use super::product::hyperbola_out;
use super::share::*;
use num_bigint::BigInt;
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(crate) fn chord_num(x0: &BigInt, y0: &BigInt, x1: &BigInt, y1: &BigInt, x: &BigInt) -> BigInt {
    y0 * (x1 - x) + y1 * (x - x0)
}

pub(crate) fn knot_pairs(knots: &[(BigInt, BigInt)]) -> Vec<(BigInt, BigInt)> {
    let mut pairs: BTreeMap<BigInt, BigInt> = BTreeMap::new();
    for (amount, out) in knots {
        if amount.is_positive() && out >= &BigInt::zero() {
            pairs.insert(amount.clone(), out.clone());
        }
    }
    pairs.into_iter().collect()
}

pub(crate) fn piece_index(xs: &[BigInt], amount: &BigInt) -> (usize, usize) {
    let mut lo = 0;
    let mut hi = xs.len() - 1;
    while hi - lo > 1 {
        let mid = (lo + hi) / 2;
        if &xs[mid] <= amount {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo, hi)
}

pub(crate) fn origin_ray(x0: &BigInt, y0: &BigInt, amount: &BigInt) -> BigInt {
    if x0.is_zero() {
        BigInt::zero()
    } else {
        floor_div(&(y0 * amount), x0)
    }
}

pub(crate) fn secant_tangents(xs: &[BigInt], ys: &[BigInt]) -> Vec<BigRational> {
    let mut secants = Vec::with_capacity(xs.len().saturating_sub(1));
    for i in 0..xs.len().saturating_sub(1) {
        let span = &xs[i + 1] - &xs[i];
        if span.is_zero() {
            secants.push(BigRational::zero());
        } else {
            secants.push(BigRational::new(&ys[i + 1] - &ys[i], span));
        }
    }
    let mut tangents = vec![BigRational::zero(); xs.len()];
    if secants.is_empty() {
        return tangents;
    }
    tangents[0] = secants[0].clone();
    tangents[xs.len() - 1] = secants[secants.len() - 1].clone();
    for i in 1..xs.len().saturating_sub(1) {
        if &secants[i - 1] * &secants[i] <= BigRational::zero() {
            tangents[i] = BigRational::zero();
            continue;
        }
        let left = BigRational::from_integer(&xs[i + 1] - &xs[i]);
        let right = BigRational::from_integer(&xs[i] - &xs[i - 1]);
        tangents[i] = (&left + &right) / (&left / &secants[i - 1] + &right / &secants[i]);
    }
    let three = BigRational::from_integer(BigInt::from(3));
    for i in 0..secants.len() {
        let delta = secants[i].clone();
        if delta.is_zero() {
            tangents[i] = BigRational::zero();
            tangents[i + 1] = BigRational::zero();
            continue;
        }
        if &tangents[i] / &delta > three {
            tangents[i] = &three * &delta;
        }
        if &tangents[i + 1] / &delta > three {
            tangents[i + 1] = &three * &delta;
        }
    }
    tangents
}

pub(crate) fn hermite_on_span(
    y0: &BigRational,
    y1: &BigRational,
    span: &BigInt,
    m0: &BigRational,
    m1: &BigRational,
    dt: &BigInt,
) -> BigRational {
    let t = BigRational::new(dt.clone(), span.clone());
    let one = BigRational::one();
    let two = BigRational::from_integer(BigInt::from(2));
    let three = BigRational::from_integer(BigInt::from(3));
    let one_t = &one - &t;
    let h00 = (&one + &two * &t) * &one_t * &one_t;
    let h10 = &t * &one_t * &one_t;
    let h01 = &t * &t * (&three - &two * &t);
    let h11 = &t * &t * (&t - &one);
    let span_r = BigRational::from_integer(span.clone());
    h00 * y0 + h10 * &span_r * m0 + h01 * y1 + h11 * span_r * m1
}

pub(crate) fn chord_quote(knots: &[(BigInt, BigInt)]) -> Option<Predict> {
    let pairs = knot_pairs(knots);
    if pairs.len() < 2 {
        return None;
    }
    let xs: Vec<BigInt> = pairs.iter().map(|(amount, _)| amount.clone()).collect();
    let ys: Vec<BigInt> = pairs.iter().map(|(_, out)| out.clone()).collect();
    Some(Arc::new(move |amount| {
        if amount.is_negative() {
            return None;
        }
        if amount <= xs[0] {
            return Some(origin_ray(&xs[0], &ys[0], &amount));
        }
        if amount >= xs[xs.len() - 1] {
            return Some(ys[ys.len() - 1].clone());
        }
        let (lo, hi) = piece_index(&xs, &amount);
        let span = &xs[hi] - &xs[lo];
        if span <= BigInt::zero() {
            return Some(ys[lo].clone());
        }
        Some(&ys[lo] + floor_div(&((&ys[hi] - &ys[lo]) * (&amount - &xs[lo])), &span))
    }))
}

pub(crate) fn chord_in(knots: &[(BigInt, BigInt)]) -> Option<Predict> {
    let pairs = knot_pairs(knots);
    if pairs.len() < 2 {
        return None;
    }
    let xs: Vec<BigInt> = pairs.iter().map(|(amount, _)| amount.clone()).collect();
    let ys: Vec<BigInt> = pairs.iter().map(|(_, out)| out.clone()).collect();
    Some(Arc::new(move |output| {
        if output.is_negative() || output > ys[ys.len() - 1] {
            return None;
        }
        if output <= ys[0] {
            if ys[0].is_zero() {
                return if output.is_zero() {
                    Some(BigInt::zero())
                } else {
                    None
                };
            }
            return Some(floor_div(&(&xs[0] * &output), &ys[0]));
        }
        for i in 0..xs.len() - 1 {
            if ys[i + 1] > ys[i] && ys[i] <= output && output <= ys[i + 1] {
                let span = &ys[i + 1] - &ys[i];
                return Some(
                    &xs[i] + floor_div(&((&xs[i + 1] - &xs[i]) * (&output - &ys[i])), &span),
                );
            }
        }
        if output == ys[ys.len() - 1] {
            return Some(xs[xs.len() - 1].clone());
        }
        None
    }))
}

pub(crate) fn cubic_quote(knots: &[(BigInt, BigInt)]) -> Option<Predict> {
    let pairs = knot_pairs(knots);
    if pairs.len() < 2 {
        return None;
    }
    let xs: Vec<BigInt> = pairs.iter().map(|(amount, _)| amount.clone()).collect();
    let ys: Vec<BigInt> = pairs.iter().map(|(_, out)| out.clone()).collect();
    let tangents = secant_tangents(&xs, &ys);
    Some(Arc::new(move |amount| {
        if amount.is_negative() {
            return None;
        }
        if amount <= xs[0] {
            return Some(origin_ray(&xs[0], &ys[0], &amount));
        }
        if amount >= xs[xs.len() - 1] {
            return Some(ys[ys.len() - 1].clone());
        }
        let (lo, hi) = piece_index(&xs, &amount);
        if amount == xs[lo] {
            return Some(ys[lo].clone());
        }
        let span = &xs[hi] - &xs[lo];
        if span <= BigInt::zero() {
            return Some(ys[lo].clone());
        }
        let value = hermite_on_span(
            &BigRational::from_integer(ys[lo].clone()),
            &BigRational::from_integer(ys[hi].clone()),
            &span,
            &tangents[lo],
            &tangents[hi],
            &(&amount - &xs[lo]),
        );
        if value <= BigRational::zero() {
            return Some(BigInt::zero());
        }
        Some(trunc_rat(&value))
    }))
}

pub(crate) fn hermite(
    knots: &[(BigInt, BigInt)],
    alpha: &BigRational,
) -> Option<Arc<dyn Fn(BigInt) -> BigRational + Send + Sync>> {
    if knots.len() < 2 {
        return None;
    }
    for i in 0..knots.len() - 1 {
        if knots[i + 1].0 <= knots[i].0 {
            return None;
        }
    }
    let xs: Vec<BigInt> = knots.iter().map(|(amount, _)| amount.clone()).collect();
    let ys: Vec<BigInt> = knots.iter().map(|(_, out)| out.clone()).collect();
    let scale = alpha.clone();
    let tangents: Vec<BigRational> = secant_tangents(&xs, &ys)
        .into_iter()
        .map(|value| value * &scale)
        .collect();
    Some(Arc::new(move |amount| {
        if amount <= xs[0] {
            return BigRational::from_integer(ys[0].clone());
        }
        if amount >= xs[xs.len() - 1] {
            return BigRational::from_integer(ys[ys.len() - 1].clone());
        }
        let (lo, hi) = piece_index(&xs, &amount);
        if amount == xs[lo] {
            return BigRational::from_integer(ys[lo].clone());
        }
        let span = &xs[hi] - &xs[lo];
        hermite_on_span(
            &BigRational::from_integer(ys[lo].clone()),
            &BigRational::from_integer(ys[hi].clone()),
            &span,
            &tangents[lo],
            &tangents[hi],
            &(&amount - &xs[lo]),
        )
    }))
}

pub(crate) fn residual_quote(knots: &[(BigInt, BigInt)]) -> Option<Predict> {
    let pairs = knot_pairs(knots);
    if pairs.len() < 2 {
        return None;
    }
    let (x1, y1) = pairs[0].clone();
    let (x2, y2) = pairs[pairs.len() - 1].clone();
    let mut residual = Vec::with_capacity(pairs.len());
    for (amount, out) in &pairs {
        let base = hyperbola_out(&x1, &y1, &x2, &y2, amount)?;
        residual.push((amount.clone(), out - base));
    }
    let correction = hermite(&residual, &BigRational::one())?;
    Some(Arc::new(move |amount| {
        if amount.is_negative() {
            return None;
        }
        let base = hyperbola_out(&x1, &y1, &x2, &y2, &amount)?;
        let value = BigRational::from_integer(base) + correction(amount);
        if value <= BigRational::zero() {
            return Some(BigInt::zero());
        }
        Some(trunc_rat(&value))
    }))
}

pub(crate) fn added_residual(
    base: Predict,
    knots: &[(BigInt, BigInt)],
    alpha: &BigRational,
) -> Option<Predict> {
    let mut ordered = knots.to_vec();
    ordered.sort();
    let mut collapsed: Vec<(BigInt, BigInt)> = Vec::new();
    for (amount, out) in ordered {
        if !amount.is_positive() {
            continue;
        }
        if collapsed.last().is_some_and(|(last, _)| last == &amount) {
            if let Some(slot) = collapsed.last_mut() {
                slot.1 = out;
            }
        } else {
            collapsed.push((amount, out));
        }
    }
    if collapsed.len() < 2 {
        return None;
    }
    let mut residual = Vec::with_capacity(collapsed.len());
    for (amount, out) in &collapsed {
        let pred = base(amount.clone())?;
        residual.push((amount.clone(), out - pred));
    }
    let correction = hermite(&residual, alpha)?;
    Some(Arc::new(move |amount| {
        if amount.is_negative() {
            return None;
        }
        let pred = base(amount.clone())?;
        let value = BigRational::from_integer(pred) + correction(amount);
        if value <= BigRational::zero() {
            return Some(BigInt::zero());
        }
        Some(trunc_rat(&value))
    }))
}
