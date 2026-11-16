//! Stack Newton for Curve crypto quotes.
//!
//! `Ok` is the same answer as the `BigInt` loop, including `None`. `Err(())`
//! means an intermediate did not fit in 256 bits and the caller uses that loop.

use alloy_primitives::{Uint, U256};
use num_bigint::{BigInt, Sign};
use num_traits::Signed;

type U512 = Uint<512, 8>;

const WAD: u128 = 1_000_000_000_000_000_000;
const A_MULTIPLIER: u128 = 10_000;
const TEN14: u128 = 100_000_000_000_000;

pub(super) fn y2(
    ann: &BigInt,
    gamma: &BigInt,
    x: &[BigInt],
    d: &BigInt,
    i: i64,
) -> Result<Option<BigInt>, ()> {
    if !ann.is_positive()
        || !gamma.is_positive()
        || !d.is_positive()
        || (i != 0 && i != 1)
        || x.len() < 2
    {
        return Ok(None);
    }
    let x_j = &x[(1 - i) as usize];
    if !x_j.is_positive() {
        return Ok(None);
    }
    let ann = u256(ann)?;
    let gamma = u256(gamma)?;
    let x_j = u256(x_j)?;
    let d = u256(d)?;
    let wad = U256::from(WAD);
    let two = U256::from(2u8);
    let ten14 = U256::from(TEN14);
    let y_den = x_j.checked_mul(U256::from(4u8)).ok_or(())?;
    let mut y = mul_div(d, d, y_den)?;
    let k0_i = mul_div(wad.checked_mul(two).ok_or(())?, x_j, d)?;
    let limit = div(x_j, ten14).max(div(d, ten14)).max(U256::from(100u8));
    for _ in 0..255 {
        if y.is_zero() {
            return Ok(None);
        }
        let previous = y;
        let k0 = mul3_div(k0_i, y, two, d)?;
        if k0.is_zero() {
            return Ok(None);
        }
        let g = g1(gamma, k0)?;
        if g.is_zero() {
            return Ok(None);
        }
        let mut mul1 = mul_div(wad, d, gamma)?;
        mul1 = mul_div(mul1, g, gamma)?;
        mul1 = mul3_div(mul1, g, U256::from(A_MULTIPLIER), ann)?;
        let mul2 = add(wad, mul_div(wad.checked_mul(two).ok_or(())?, k0, g)?)?;
        let xy = add(x_j, y)?;
        let mut yfprime = add(mul(wad, y)?, mul(xy, mul2)?)?;
        yfprime = add(yfprime, mul1)?;
        let dyfprime = mul(d, mul2)?;
        if yfprime < dyfprime {
            y = div(previous, two);
            continue;
        }
        yfprime -= dyfprime;
        let fprime = div(yfprime, y);
        if fprime.is_zero() {
            return Ok(None);
        }
        let head = div(mul1, fprime);
        let y_minus = add(head, mul_div(wad, add(x_j, y)?, fprime)?)?;
        let y_plus = add(
            mul_div(add(yfprime, mul(wad, d)?)?, U256::from(1u8), fprime)?,
            mul_div(mul(head, wad)?, U256::from(1u8), k0)?,
        )?;
        y = if y_plus < y_minus {
            div(previous, two)
        } else {
            y_plus - y_minus
        };
        let tol = limit.max(div(y, ten14));
        if abs_diff(y, previous) < tol {
            return Ok(Some(bigint(y)));
        }
    }
    Ok(None)
}

pub(super) fn y3(
    ann: &BigInt,
    gamma: &BigInt,
    x: &[BigInt],
    d: &BigInt,
    i: i64,
) -> Result<Option<BigInt>, ()> {
    if !ann.is_positive()
        || !gamma.is_positive()
        || !d.is_positive()
        || !(0..=2).contains(&i)
        || x.len() != 3
    {
        return Ok(None);
    }
    let ann = u256(ann)?;
    let gamma = u256(gamma)?;
    let d = u256(d)?;
    let mut ordered = [u256(&x[0])?, u256(&x[1])?, u256(&x[2])?];
    ordered[i as usize] = U256::ZERO;
    if ordered[0] < ordered[1] {
        ordered.swap(0, 1);
    }
    if ordered[0] < ordered[2] {
        ordered.swap(0, 2);
    }
    if ordered[1] < ordered[2] {
        ordered.swap(1, 2);
    }
    let wad = U256::from(WAD);
    let three = U256::from(3u8);
    let two = U256::from(2u8);
    let ten14 = U256::from(TEN14);
    let mut y = div(d, three);
    let mut k0_i = wad;
    let mut s_i = U256::ZERO;
    let limit = div(ordered[0], ten14)
        .max(div(d, ten14))
        .max(U256::from(100u8));
    for step in 2..4 {
        let known = ordered[3 - step];
        if known.is_zero() {
            return Ok(None);
        }
        y = mul_div(y, d, known.checked_mul(three).ok_or(())?)?;
        s_i = add(s_i, known)?;
    }
    for value in &ordered[..2] {
        k0_i = mul3_div(k0_i, *value, three, d)?;
    }
    for _ in 0..255 {
        if y.is_zero() || k0_i.is_zero() {
            return Ok(None);
        }
        let previous = y;
        let k0 = mul3_div(k0_i, y, three, d)?;
        if k0.is_zero() {
            return Ok(None);
        }
        let g = g1(gamma, k0)?;
        if g.is_zero() {
            return Ok(None);
        }
        let mut mul1 = mul_div(wad, d, gamma)?;
        mul1 = mul_div(mul1, g, gamma)?;
        mul1 = mul3_div(mul1, g, U256::from(A_MULTIPLIER), ann)?;
        let mul2 = add(wad, mul_div(wad.checked_mul(two).ok_or(())?, k0, g)?)?;
        let sy = add(s_i, y)?;
        let mut yfprime = add(mul(wad, y)?, mul(sy, mul2)?)?;
        yfprime = add(yfprime, mul1)?;
        let dyfprime = mul(d, mul2)?;
        if yfprime < dyfprime {
            y = div(previous, two);
            continue;
        }
        yfprime -= dyfprime;
        let fprime = div(yfprime, y);
        if fprime.is_zero() {
            return Ok(None);
        }
        let mut y_minus = div(mul1, fprime);
        let y_plus = add(
            mul_div(add(yfprime, mul(wad, d)?)?, U256::from(1u8), fprime)?,
            mul_div(mul(y_minus, wad)?, U256::from(1u8), k0)?,
        )?;
        y_minus = add(y_minus, mul_div(wad, add(s_i, y)?, fprime)?)?;
        y = if y_plus < y_minus {
            div(previous, two)
        } else {
            y_plus - y_minus
        };
        let tol = limit.max(div(y, ten14));
        if abs_diff(y, previous) < tol {
            return Ok(Some(bigint(y)));
        }
    }
    Ok(None)
}

fn g1(gamma: U256, k0: U256) -> Result<U256, ()> {
    let base = add(gamma, U256::from(WAD))?;
    if base > k0 {
        add(base - k0, U256::from(1u8))
    } else {
        add(k0 - base, U256::from(1u8))
    }
}

fn add(left: U256, right: U256) -> Result<U256, ()> {
    left.checked_add(right).ok_or(())
}

fn mul(left: U256, right: U256) -> Result<U256, ()> {
    left.checked_mul(right).ok_or(())
}

fn div(numer: U256, denom: U256) -> U256 {
    if denom.is_zero() {
        U256::ZERO
    } else {
        numer / denom
    }
}

fn mul_div(left: U256, right: U256, den: U256) -> Result<U256, ()> {
    let product: U512 = left.widening_mul(right);
    div512(product, den)
}

fn mul3_div(a: U256, b: U256, c: U256, den: U256) -> Result<U256, ()> {
    let ab: U512 = a.widening_mul(b);
    let Some(narrowed) = narrow(ab) else {
        let product = ab.checked_mul(widen(c)).ok_or(())?;
        return div512(product, den);
    };
    mul_div(narrowed, c, den)
}

fn div512(numer: U512, den: U256) -> Result<U256, ()> {
    if den.is_zero() {
        return Ok(U256::ZERO);
    }
    let (quot, _) = numer.div_rem(widen(den));
    narrow(quot).ok_or(())
}

fn widen(value: U256) -> U512 {
    let limbs = value.as_limbs();
    U512::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3], 0, 0, 0, 0])
}

fn narrow(value: U512) -> Option<U256> {
    let limbs = value.as_limbs();
    if limbs[4] != 0 || limbs[5] != 0 || limbs[6] != 0 || limbs[7] != 0 {
        return None;
    }
    Some(U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]))
}

fn abs_diff(left: U256, right: U256) -> U256 {
    if left > right {
        left - right
    } else {
        right - left
    }
}

fn u256(value: &BigInt) -> Result<U256, ()> {
    if value.sign() == Sign::Minus {
        return Err(());
    }
    let mut limbs = [0u64; 4];
    for (index, digit) in value.magnitude().iter_u64_digits().enumerate() {
        if index == 4 {
            return Err(());
        }
        limbs[index] = digit;
    }
    Ok(U256::from_limbs(limbs))
}

fn bigint(value: U256) -> BigInt {
    match u128::try_from(value) {
        Ok(small) => BigInt::from(small),
        Err(_) => BigInt::from_bytes_le(Sign::Plus, &value.to_le_bytes::<32>()),
    }
}
