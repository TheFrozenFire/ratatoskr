//! Half-even decimal series used by the logistic quote.

use super::units::*;
use bigdecimal::{BigDecimal, Context, RoundingMode};
use num_bigint::{BigInt, Sign};
use num_rational::BigRational;
use num_traits::{One, Signed, Zero};
use std::num::NonZeroU64;

pub(crate) fn he40(value: BigDecimal) -> BigDecimal {
    if value.sign() == Sign::NoSign {
        return BigDecimal::from(0);
    }
    dec_ctx().round_decimal(value)
}

pub(crate) fn he_at(value: BigDecimal, prec: u64) -> BigDecimal {
    if value.sign() == Sign::NoSign {
        return BigDecimal::from(0);
    }
    Context::new(
        NonZeroU64::new(prec).expect("precision"),
        RoundingMode::HalfEven,
    )
    .round_decimal(value)
}

pub(crate) fn he_div_at(numer: &BigDecimal, denom: &BigDecimal, prec: u64) -> BigDecimal {
    if denom.sign() == Sign::NoSign {
        return BigDecimal::from(0);
    }
    let guard = prec.saturating_add(8).max(16);
    let ctx = Context::new(
        NonZeroU64::new(guard).expect("precision"),
        RoundingMode::HalfEven,
    );
    he_at(numer * denom.inverse_with_context(&ctx), prec)
}

pub(crate) fn rat_dec(value: &BigRational) -> BigDecimal {
    let numer = BigDecimal::from(value.numer().clone());
    let denom = BigDecimal::from(value.denom().clone());
    he_div_at(&numer, &denom, 40)
}

pub(crate) fn trunc_dec(value: &BigDecimal) -> BigInt {
    let (digits, scale) = value.as_bigint_and_exponent();
    let exp = if scale <= 0 { -scale } else { scale };
    let Ok(exp) = u32::try_from(exp) else {
        return BigInt::zero();
    };
    // Past this, the integer part is not a token amount.
    if exp > 4_096 {
        return BigInt::zero();
    }
    if scale <= 0 {
        &digits * pow10(exp)
    } else {
        digits / pow10(exp)
    }
}

pub(crate) fn ln2_he(prec: u64) -> BigDecimal {
    let text = "693147180559945309417232121458176568075500134360255254120680009493393621969694715605863326996418687542001481020570685733685520235758130557032670751635";
    let keep = (prec as usize + 8).min(text.len());
    let digits = text[..keep].parse::<BigInt>().expect("ln2");
    he_at(BigDecimal::new(digits, keep as i64), prec)
}

/// Divide rounding half-even. `denom` is positive.
pub(crate) fn round_div(numer: &BigInt, denom: &BigInt) -> BigInt {
    if !denom.is_positive() {
        return BigInt::zero();
    }
    let q = numer / denom;
    let r = numer % denom;
    let twice = if r.is_negative() { -&r * 2 } else { &r * 2 };
    if &twice < denom {
        q
    } else if &twice > denom || !(&q % 2u32).is_zero() {
        if numer.is_negative() {
            q - 1
        } else {
            q + 1
        }
    } else {
        q
    }
}

pub(crate) fn fixed_of(value: &BigDecimal, scale: u32) -> BigInt {
    let (digits, exp) = value.as_bigint_and_exponent();
    let shift = scale as i64 - exp;
    if shift >= 0 {
        digits * pow10(shift as u32)
    } else {
        round_div(&digits, &pow10((-shift) as u32))
    }
}

/// Natural log at precision 40, half-even. Logistic quotes use this path.
/// The series runs in fixed point so a wave search stays on the same integers
/// without building a decimal context on every term.
pub(crate) fn ln_he(value: &BigDecimal) -> BigDecimal {
    if value.sign() != Sign::Plus {
        return BigDecimal::from(0);
    }
    const SCALE: u32 = 96;
    let one = pow10(SCALE);
    let mut reduced = fixed_of(value, SCALE);
    if !reduced.is_positive() {
        return BigDecimal::from(0);
    }
    let two = &one * 2;
    let half = &pow10(SCALE - 1) * 5;
    let ln2 = ln2_scaled();
    let mut extra = BigInt::zero();
    for _ in 0..20_000 {
        if reduced <= two {
            break;
        }
        reduced = round_div(&reduced, &BigInt::from(2));
        extra += ln2;
    }
    for _ in 0..20_000 {
        if reduced >= half {
            break;
        }
        reduced *= 2;
        extra -= ln2;
    }
    let z = round_div(&((&reduced - &one) * &one), &(&reduced + &one));
    let z2 = round_div(&(&z * &z), &one);
    let mut term = z.clone();
    let mut sum = z;
    let mut n = BigInt::from(1);
    for _ in 0..300 {
        term = round_div(&(&term * &z2), &one);
        n += 2;
        let piece = round_div(&term, &n);
        if piece.is_zero() {
            break;
        }
        sum += piece;
    }
    he40(BigDecimal::new(&sum * 2 + extra, SCALE as i64))
}

pub(crate) fn exp_fixed(value: &BigDecimal) -> BigDecimal {
    const SCALE: u32 = 96;
    let one = pow10(SCALE);
    let mut x = fixed_of(value, SCALE);
    let ln2 = ln2_scaled();
    let mut k: i32 = 0;
    for _ in 0..400 {
        if x <= *ln2 {
            break;
        }
        x -= ln2;
        k += 1;
    }
    for _ in 0..400 {
        if x >= -ln2 {
            break;
        }
        x += ln2;
        k -= 1;
    }
    let mut term = one.clone();
    let mut sum = one.clone();
    for n in 1..240 {
        term = round_div(&(&term * &x), &one);
        term = round_div(&term, &BigInt::from(n));
        if term.is_zero() {
            break;
        }
        sum += &term;
    }
    if k > 0 {
        sum <<= k as u32;
    } else if k < 0 {
        sum = round_div(&sum, &(BigInt::one() << ((-k) as usize)));
    }
    he40(BigDecimal::new(sum, SCALE as i64))
}

pub(crate) fn ln2_scaled() -> &'static BigInt {
    static LN2: std::sync::OnceLock<BigInt> = std::sync::OnceLock::new();
    LN2.get_or_init(|| fixed_of(&ln2_he(96), 96))
}

pub(crate) fn ln1exp(value: &BigDecimal) -> BigDecimal {
    let forty = BigDecimal::from(40);
    if value > &forty {
        return value.clone();
    }
    if value < &BigDecimal::from(-40) {
        return BigDecimal::from(0);
    }
    ln_he(&he40(BigDecimal::from(1) + exp_fixed(value)))
}
