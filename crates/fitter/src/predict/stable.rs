//! Stableswap invariant quotes.

use super::share::*;
use alloy_primitives::{Uint, U256};
use num_bigint::{BigInt, Sign};
use num_traits::{One, Signed, ToPrimitive, Zero};

type U512 = Uint<512, 8>;

enum Fitted<T> {
    Ready(T),
    Spill,
}

pub(crate) fn stable_d(xp: &[BigInt], amp: &BigInt) -> Option<BigInt> {
    match stable_d_fitted(xp, amp) {
        Fitted::Ready(value) => value,
        Fitted::Spill => stable_d_slow(xp, amp),
    }
}

fn stable_d_slow(xp: &[BigInt], amp: &BigInt) -> Option<BigInt> {
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
    match stable_y_fitted(i, j, x, xp, amp, d) {
        Fitted::Ready(value) => value,
        Fitted::Spill => stable_y_slow(i, j, x, xp, amp, d),
    }
}

fn stable_y_slow(
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

fn stable_d_fitted(xp: &[BigInt], amp: &BigInt) -> Fitted<Option<BigInt>> {
    match stable_d_result(xp, amp) {
        Ok(value) => Fitted::Ready(value),
        Err(stop) => stop,
    }
}

fn stable_d_result(xp: &[BigInt], amp: &BigInt) -> Result<Option<BigInt>, Fitted<Option<BigInt>>> {
    let n_len = xp.len();
    if n_len < 2 || !amp.is_positive() {
        return Ok(None);
    }
    let n = U256::from(n_len);
    let mut total = U256::ZERO;
    let mut xs = Vec::with_capacity(n_len);
    for value in xp {
        let value = positive_u256(value)?;
        total = match total.checked_add(value) {
            Some(total) => total,
            None => return Err(Fitted::Spill),
        };
        xs.push(value);
    }
    if total.is_zero() {
        return Ok(None);
    }
    let amp = positive_u256(amp)?;
    let Some(ann) = amp.checked_mul(n) else {
        return Err(Fitted::Spill);
    };
    let mut d = total;
    for _ in 0..255 {
        let mut d_p = d;
        for value in &xs {
            let Some(den) = value.checked_mul(n) else {
                return Err(Fitted::Spill);
            };
            d_p = ready_u256(mul_div(d_p, d, den))?;
        }
        let previous = d;
        let Some(ann_total) = ann.checked_mul(total) else {
            return Err(Fitted::Spill);
        };
        let Some(d_p_n) = d_p.checked_mul(n) else {
            return Err(Fitted::Spill);
        };
        let Some(sum) = ann_total.checked_add(d_p_n) else {
            return Err(Fitted::Spill);
        };
        let Some(ann_less) = ann.checked_sub(U256::from(1u8)) else {
            return Ok(None);
        };
        let Some(left) = ann_less.checked_mul(d) else {
            return Err(Fitted::Spill);
        };
        let Some(n_more) = n.checked_add(U256::from(1u8)) else {
            return Err(Fitted::Spill);
        };
        let Some(right) = n_more.checked_mul(d_p) else {
            return Err(Fitted::Spill);
        };
        let Some(den) = left.checked_add(right) else {
            return Err(Fitted::Spill);
        };
        if den.is_zero() {
            return Ok(None);
        }
        let (quot, _) = sum.widening_mul(d).div_rem(widen(den));
        d = match narrow(quot) {
            Some(quot) => quot,
            None => return Err(Fitted::Spill),
        };
        if within_one(d, previous) {
            return Ok(Some(bigint_of(d)));
        }
    }
    Ok(None)
}

fn stable_y_fitted(
    i: i64,
    j: i64,
    x: &BigInt,
    xp: &[BigInt],
    amp: &BigInt,
    d: &BigInt,
) -> Fitted<Option<BigInt>> {
    match stable_y_result(i, j, x, xp, amp, d) {
        Ok(value) => Fitted::Ready(value),
        Err(stop) => stop,
    }
}

fn stable_y_result(
    i: i64,
    j: i64,
    x: &BigInt,
    xp: &[BigInt],
    amp: &BigInt,
    d: &BigInt,
) -> Result<Option<BigInt>, Fitted<Option<BigInt>>> {
    let n_len = xp.len() as i64;
    if i == j || i < 0 || j < 0 || i >= n_len || j >= n_len {
        return Ok(None);
    }
    let amp = positive_u256(amp)?;
    let d = positive_u256(d)?;
    let x = positive_u256(x)?;
    let n = U256::from(n_len as u64);
    let Some(ann) = amp.checked_mul(n) else {
        return Err(Fitted::Spill);
    };
    let mut c = d;
    let mut s = U256::ZERO;
    for (k, balance) in xp.iter().enumerate() {
        if k as i64 == j {
            continue;
        }
        let x_k = if k as i64 == i {
            x
        } else {
            positive_u256(balance)?
        };
        s = match s.checked_add(x_k) {
            Some(s) => s,
            None => return Err(Fitted::Spill),
        };
        let Some(den) = x_k.checked_mul(n) else {
            return Err(Fitted::Spill);
        };
        c = ready_u256(mul_div(c, d, den))?;
    }
    let Some(ann_n) = ann.checked_mul(n) else {
        return Err(Fitted::Spill);
    };
    c = ready_u256(mul_div(c, d, ann_n))?;
    let b = match s.checked_add(ready_u256(div_small(d, ann))?) {
        Some(b) => b,
        None => return Err(Fitted::Spill),
    };
    let mut y = d;
    for _ in 0..255 {
        let previous = y;
        let Some(two_y) = y.checked_mul(U256::from(2u8)) else {
            return Err(Fitted::Spill);
        };
        let Some(sum) = two_y.checked_add(b) else {
            return Err(Fitted::Spill);
        };
        if sum <= d {
            return Ok(None);
        }
        let den = sum - d;
        let numer = match y.widening_mul(y).checked_add(widen(c)) {
            Some(numer) => numer,
            None => return Err(Fitted::Spill),
        };
        let (quot, _) = numer.div_rem(widen(den));
        y = match narrow(quot) {
            Some(quot) => quot,
            None => return Err(Fitted::Spill),
        };
        if within_one(y, previous) {
            return Ok(Some(bigint_of(y)));
        }
    }
    Ok(None)
}

fn positive_u256(value: &BigInt) -> Result<U256, Fitted<Option<BigInt>>> {
    if value.sign() == Sign::Minus || value.is_zero() {
        return Err(Fitted::Ready(None));
    }
    u256_of(value).ok_or(Fitted::Spill)
}

fn ready_u256(value: Fitted<U256>) -> Result<U256, Fitted<Option<BigInt>>> {
    match value {
        Fitted::Ready(value) => Ok(value),
        Fitted::Spill => Err(Fitted::Spill),
    }
}

fn mul_div(left: U256, right: U256, den: U256) -> Fitted<U256> {
    if den.is_zero() {
        return Fitted::Ready(U256::ZERO);
    }
    let (quot, _) = left.widening_mul(right).div_rem(widen(den));
    match narrow(quot) {
        Some(quot) => Fitted::Ready(quot),
        None => Fitted::Spill,
    }
}

fn div_small(numer: U256, den: U256) -> Fitted<U256> {
    if den.is_zero() {
        return Fitted::Ready(U256::ZERO);
    }
    Fitted::Ready(numer / den)
}

fn within_one(left: U256, right: U256) -> bool {
    let gap = if left > right {
        left - right
    } else {
        right - left
    };
    gap <= U256::from(1u8)
}

fn u256_of(value: &BigInt) -> Option<U256> {
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

fn bigint_of(value: U256) -> BigInt {
    match u128::try_from(value) {
        Ok(small) => BigInt::from(small),
        Err(_) => BigInt::from_bytes_le(Sign::Plus, &value.to_le_bytes::<32>()),
    }
}

fn widen(value: U256) -> U512 {
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
