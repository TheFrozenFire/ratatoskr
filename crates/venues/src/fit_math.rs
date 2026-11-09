//! Integer quotes for one pool direction.
//!
//! Amounts stay integers. Each quote is the output of one exact input.
//! `//` is floor division. `int(Fraction)` truncates toward zero.

#![allow(dead_code)]
#![allow(clippy::too_many_arguments, clippy::explicit_auto_deref)]

use bigdecimal::{BigDecimal, Context, RoundingMode};
use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_rational::BigRational;
use num_traits::{One, Signed, ToPrimitive, Zero};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::num::NonZeroU64;
use std::sync::{Arc, LazyLock};

pub(crate) type Predict = Arc<dyn Fn(BigInt) -> Option<BigInt> + Send + Sync>;

pub(crate) static WAD: LazyLock<BigInt> = LazyLock::new(|| pow10(18));
pub(crate) static FEE_1E10: LazyLock<BigInt> = LazyLock::new(|| pow10(10));
pub(crate) static A_MULTIPLIER: LazyLock<BigInt> = LazyLock::new(|| BigInt::from(10_000));
pub(crate) static Q96: LazyLock<BigInt> = LazyLock::new(|| BigInt::one() << 96u32);
pub(crate) static MIN_SQRT: LazyLock<BigInt> = LazyLock::new(|| BigInt::from(4_295_128_739u64));
pub(crate) static MAX_SQRT: LazyLock<BigInt> = LazyLock::new(|| {
    parse_dec("1461446703485210103287273052203988822378723970342")
});
pub(crate) static MAX_TICK: LazyLock<BigInt> = LazyLock::new(|| BigInt::from(887_272));
pub(crate) static PRECISIONS: LazyLock<[BigInt; 4]> =
    LazyLock::new(|| [BigInt::one(), pow10(10), pow10(12), pow10(16)]);
pub(crate) static DUST: LazyLock<BigInt> = LazyLock::new(|| BigInt::from(1_000));

// 1.0001^(2^bit), scaled by 2^128. Bit 0 is the odd-tick seed.
pub(crate) static TICK_BITS: LazyLock<Vec<BigInt>> = LazyLock::new(|| {
    const HEX: [&str; 20] = [
        "fffcb933bd6fad37aa2d162d1a594001",
        "fff97272373d413259a46990580e213a",
        "fff2e50f5f656932ef12357cf3c7fdcc",
        "ffe5caca7e10e4e61c3624eaa0941cd0",
        "ffcb9843d60f6159c9db58835c926644",
        "ff973b41fa98c081472e6896dfb254c0",
        "ff2ea16466c96a3843ec78b326b52861",
        "fe5dee046a99a2a811c461f1969c3053",
        "fcbe86c7900a88aedcffc83b479aa3a4",
        "f987a7253ac413176f2b074cf7815e54",
        "f3392b0822b70005940c7a398e4b70f3",
        "e7159475a2c29b7443b29c7fa6e889d9",
        "d097f3bdfd2022b8845ad8f792aa5825",
        "a9f746462d870fdf8a65dc1f90e061e5",
        "70d869a156d2a1b890bb3df62baf32f7",
        "31be135f97d08fd981231505542fcfa6",
        "9aa508b5b7a84e1c677de54f3e99bc9",
        "5d6af8dedb81196699c329225ee604",
        "2216e584f5fa1ea926041bedfe98",
        "48a170391f7dc42444e8fa2",
    ];
    HEX.iter()
        .map(|text| parse_hex(text))
        .collect()
});

static LN2: LazyLock<BigRational> = LazyLock::new(ln2_series);

fn parse_dec(text: &str) -> BigInt {
    match BigInt::parse_bytes(text.as_bytes(), 10) {
        Some(value) => value,
        None => BigInt::zero(),
    }
}

fn parse_hex(text: &str) -> BigInt {
    match BigInt::parse_bytes(text.as_bytes(), 16) {
        Some(value) => value,
        None => BigInt::zero(),
    }
}

fn pow10(exp: u64) -> BigInt {
    // A quote scale past this is not a token amount. Capping keeps a wild
    // input from asking for 10^(2^32).
    let exp = u32::try_from(exp).unwrap_or(4_096).min(4_096);
    BigInt::from(10u32).pow(exp)
}

fn floor_div(numer: &BigInt, denom: &BigInt) -> BigInt {
    if denom.is_zero() {
        BigInt::zero()
    } else {
        numer.div_floor(denom)
    }
}

fn prec_80() -> NonZeroU64 {
    match NonZeroU64::new(80) {
        Some(value) => value,
        None => NonZeroU64::MIN,
    }
}

fn context_floor(value: BigDecimal) -> BigDecimal {
    Context::new(prec_80(), RoundingMode::Floor).round_decimal(value)
}

fn floor_log10_int(n: &BigInt) -> u64 {
    let text = n.to_str_radix(10);
    let text = text.trim_start_matches('-');
    text.len() as u64 - 1
}

fn cmp_pow10(numer: &BigInt, denom: &BigInt, exp: i64) -> Ordering {
    if exp >= 0 {
        numer.cmp(&(denom * pow10(exp as u64)))
    } else {
        (numer * pow10((-exp) as u64)).cmp(denom)
    }
}

fn floor_log10_ratio(numer: &BigInt, denom: &BigInt) -> i64 {
    let mut exp = floor_log10_int(numer) as i64 - floor_log10_int(denom) as i64;
    loop {
        if cmp_pow10(numer, denom, exp) == Ordering::Less {
            exp -= 1;
            continue;
        }
        if cmp_pow10(numer, denom, exp + 1) != Ordering::Less {
            exp += 1;
            continue;
        }
        return exp;
    }
}

fn cmp_pow2(numer: &BigInt, denom: &BigInt, exp: i64) -> Ordering {
    if exp >= 0 {
        numer.cmp(&(denom * (BigInt::one() << exp as u32)))
    } else {
        (numer * (BigInt::one() << ((-exp) as u32))).cmp(denom)
    }
}

fn floor_log2_ratio(numer: &BigInt, denom: &BigInt) -> i64 {
    let mut exp = numer.bits() as i64 - denom.bits() as i64;
    loop {
        if cmp_pow2(numer, denom, exp) == Ordering::Less {
            exp -= 1;
            continue;
        }
        if cmp_pow2(numer, denom, exp + 1) != Ordering::Less {
            exp += 1;
            continue;
        }
        return exp;
    }
}

/// Floor to `prec` significant digits. The denominator is positive.
fn round_sig_floor_dec(numer: &BigInt, denom: &BigInt, prec: u64) -> BigDecimal {
    if !numer.is_positive() {
        return BigDecimal::from_bigint(BigInt::zero(), 0);
    }
    let exp = floor_log10_ratio(numer, denom);
    let shift = prec as i64 - 1 - exp;
    let sig = if shift >= 0 {
        (numer * pow10(shift as u64)).div_floor(denom)
    } else {
        numer.div_floor(&(denom * pow10((-shift) as u64)))
    };
    if shift >= 0 {
        BigDecimal::from_bigint(sig, shift)
    } else {
        BigDecimal::from_bigint(sig * pow10((-shift) as u64), 0)
    }
}

fn rat_from_decimal(value: &BigDecimal) -> BigRational {
    let (digits, scale) = value.as_bigint_and_exponent();
    if scale > 0 {
        BigRational::new(digits, pow10(scale as u64))
    } else if scale < 0 {
        BigRational::from_integer(digits * pow10((-scale) as u64))
    } else {
        BigRational::from_integer(digits)
    }
}

fn floor80_ratio(value: &BigRational) -> BigRational {
    if !value.numer().is_positive() {
        return BigRational::zero();
    }
    let dec = round_sig_floor_dec(value.numer(), value.denom(), 80);
    rat_from_decimal(&context_floor(dec))
}

fn floor_rat(value: &BigRational) -> BigInt {
    value.numer().div_floor(value.denom())
}

fn trunc_rat(value: &BigRational) -> BigInt {
    value.numer() / value.denom()
}

fn ln2_series() -> BigRational {
    let w = BigRational::new(BigInt::one(), BigInt::from(3));
    let w2 = &w * &w;
    let mut term = w;
    let mut sum = BigRational::zero();
    for n in 0..320 {
        sum += &term / BigRational::from_integer(BigInt::from(2 * n + 1));
        term *= &w2;
    }
    BigRational::from_integer(BigInt::from(2)) * sum
}

fn ln_unit(y: &BigRational) -> BigRational {
    let one = BigRational::one();
    if y >= &one {
        return BigRational::zero();
    }
    let w = (&one - y) / (&one + y);
    let w2 = &w * &w;
    let mut term = w;
    let mut sum = BigRational::zero();
    for n in 0..320 {
        sum += &term / BigRational::from_integer(BigInt::from(2 * n + 1));
        term *= &w2;
    }
    -BigRational::from_integer(BigInt::from(2)) * sum
}

fn ln_pos(x: &BigRational) -> BigRational {
    let k = floor_log2_ratio(x.numer(), x.denom());
    let shift = k + 1;
    let y = if shift >= 0 {
        x / BigRational::from_integer(BigInt::one() << shift as u32)
    } else {
        x * BigRational::from_integer(BigInt::one() << ((-shift) as u32))
    };
    ln_unit(&y) + BigRational::from_integer(BigInt::from(shift)) * LN2.clone()
}

fn ln_ten() -> BigRational {
    ln_unit(&BigRational::new(BigInt::from(5), BigInt::from(8)))
        + BigRational::from_integer(BigInt::from(4)) * LN2.clone()
}

fn exp_series(t: &BigRational) -> BigRational {
    let mut term = BigRational::one();
    let mut sum = BigRational::one();
    for n in 1..320 {
        term *= t;
        term /= BigRational::from_integer(BigInt::from(n));
        sum += &term;
    }
    sum
}

fn scale_int(digits: BigInt, exp: i64) -> BigRational {
    if exp >= 0 {
        BigRational::from_integer(digits * pow10(exp as u64))
    } else {
        BigRational::new(digits, pow10((-exp) as u64))
    }
}

fn exp_floor80(t: &BigRational) -> BigRational {
    if t.is_zero() {
        return BigRational::one();
    }
    let z = t / ln_ten();
    let mut n = floor_rat(&z).to_i64().unwrap_or(0);
    let frac = &z - BigRational::from_integer(BigInt::from(n));
    let mut sig = if frac.is_zero() {
        BigRational::one()
    } else {
        exp_series(&(&frac * ln_ten()))
    };
    let ten = BigRational::from_integer(BigInt::from(10));
    for _ in 0..8 {
        if sig < ten {
            break;
        }
        sig /= &ten;
        n += 1;
    }
    for _ in 0..8 {
        if sig >= BigRational::one() {
            break;
        }
        sig *= &ten;
        n -= 1;
    }
    let digits = floor_rat(&(&sig * BigRational::from_integer(pow10(79))));
    floor80_ratio(&scale_int(digits, n - 79))
}

fn rat_pow(base: &BigRational, mut exp: u32) -> BigRational {
    let mut result = BigRational::one();
    let mut b = base.clone();
    while exp > 0 {
        if exp & 1 == 1 {
            result *= &b;
        }
        exp >>= 1;
        if exp > 0 {
            b = &b * &b;
        }
    }
    result
}

fn small_u32(exp: &BigRational) -> Option<u32> {
    if exp.denom() != &BigInt::one() || exp.numer().sign() == Sign::Minus {
        return None;
    }
    let k = exp.numer().to_u32()?;
    // Exact power of an 80-digit base. A balancer weight ratio fits. Past this
    // the log path is the same 80-digit floor, and the integer stays bounded.
    if k > 4_096 {
        return None;
    }
    Some(k)
}

fn bd_at(prec: u64) -> Context {
    Context::new(NonZeroU64::new(prec).expect("precision"), RoundingMode::Floor)
}

fn bd_floor(value: BigDecimal, prec: u64) -> BigDecimal {
    if value.sign() == Sign::NoSign {
        return BigDecimal::from(0);
    }
    bd_at(prec).round_decimal(value)
}

fn ratio_bd(value: &BigRational, prec: u64) -> BigDecimal {
    round_sig_floor_dec(value.numer(), value.denom(), prec)
}

fn ln2_bd(prec: u64) -> BigDecimal {
    let text = "693147180559945309417232121458176568075500134360255254120680009493393621969694715605863326996418687542001481020570685733685520235758130557032670751635";
    let keep = prec.min(text.len() as u64) as usize;
    let digits = parse_dec(&text[..keep]);
    bd_floor(BigDecimal::new(digits, keep as i64), prec)
}

fn bd_div(numer: &BigDecimal, denom: &BigDecimal, prec: u64) -> BigDecimal {
    let ctx = bd_at(prec + 4);
    bd_floor(numer * denom.inverse_with_context(&ctx), prec)
}

fn ln_bd(value: &BigDecimal, prec: u64) -> Option<BigDecimal> {
    if value.sign() != Sign::Plus {
        return None;
    }
    let work = prec + 8;
    let mut reduced = bd_floor(value.clone(), work);
    let two = BigDecimal::from(2);
    let half = BigDecimal::new(BigInt::from(5), 1);
    let ln2 = ln2_bd(work);
    let mut extra = BigDecimal::from(0);
    for _ in 0..10_000 {
        if reduced <= two {
            break;
        }
        reduced = bd_div(&reduced, &two, work);
        extra += &ln2;
    }
    for _ in 0..10_000 {
        if reduced >= half {
            break;
        }
        reduced = bd_floor(&reduced * &two, work);
        extra -= &ln2;
    }
    if reduced < half || reduced > two {
        return None;
    }
    let one = BigDecimal::from(1);
    let z = bd_div(&(reduced.clone() - &one), &(reduced + &one), work);
    let z2 = bd_floor(&z * &z, work);
    let mut term = z.clone();
    let mut sum = z;
    let mut n = BigDecimal::from(1);
    for _ in 0..(work as usize * 2) {
        term = bd_floor(&term * &z2, work);
        n += BigDecimal::from(2);
        let piece = bd_div(&term, &n, work);
        if piece.sign() == Sign::NoSign {
            break;
        }
        sum += piece;
    }
    Some(bd_floor(extra + bd_floor(&sum * &two, work), prec))
}

fn decimal_pow(base: &BigRational, exp: &BigRational) -> BigRational {
    if !base.numer().is_positive() {
        return BigRational::zero();
    }
    if exp.is_zero() || base == &BigRational::one() {
        return BigRational::one();
    }
    if let Some(k) = small_u32(exp) {
        return floor80_ratio(&rat_pow(base, k));
    }
    let work = 100u64;
    let base_bd = ratio_bd(base, work);
    let exp_bd = ratio_bd(exp, work);
    let Some(ln) = ln_bd(&base_bd, work) else {
        return BigRational::zero();
    };
    let ctx = bd_at(work);
    let raised = bd_floor(&ln * &exp_bd, work).exp_with_context(&ctx);
    rat_from_decimal(&bd_floor(raised, 80))
}

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

pub(crate) fn chord_num(x0: &BigInt, y0: &BigInt, x1: &BigInt, y1: &BigInt, x: &BigInt) -> BigInt {
    y0 * (x1 - x) + y1 * (x - x0)
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

fn samples_ok(x1: &BigInt, y1: &BigInt, x2: &BigInt, y2: &BigInt) -> bool {
    x1.is_positive() && x2 > x1 && y1.is_positive() && y2 >= y1 && hyp_D(x1, y1, x2, y2).is_positive()
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

pub(crate) fn knot_pairs(knots: &[(BigInt, BigInt)]) -> Vec<(BigInt, BigInt)> {
    let mut pairs: BTreeMap<BigInt, BigInt> = BTreeMap::new();
    for (amount, out) in knots {
        if amount.is_positive() && out >= &BigInt::zero() {
            pairs.insert(amount.clone(), out.clone());
        }
    }
    pairs.into_iter().collect()
}

fn piece_index(xs: &[BigInt], amount: &BigInt) -> (usize, usize) {
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

fn origin_ray(x0: &BigInt, y0: &BigInt, amount: &BigInt) -> BigInt {
    if x0.is_zero() {
        BigInt::zero()
    } else {
        floor_div(&(y0 * amount), x0)
    }
}

fn secant_tangents(xs: &[BigInt], ys: &[BigInt]) -> Vec<BigRational> {
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

fn hermite_on_span(
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

fn hermite(
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
    if i == j || i < 0 || j < 0 || i >= n_len || j >= n_len || !amp.is_positive() || !d.is_positive() || !x.is_positive()
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
        let x_k = if k as i64 == i { x.clone() } else { balance.clone() };
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
                &floor_div(&(&xp[i as usize] + &xp[i as usize] + &paid), &BigInt::from(2)),
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

fn g1(gamma: &BigInt, k0: &BigInt) -> BigInt {
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
    if !ann.is_positive() || !gamma.is_positive() || !d.is_positive() || (i != 0 && i != 1) || x.len() < 2
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
        let y_minus =
            floor_div(&mul1, &fprime) + floor_div(&(&*WAD * (x_j + &y)), &fprime);
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

fn flat_fee(xp: &[BigInt], mid_fee: &BigInt, out_fee: &BigInt, fee_gamma: &BigInt) -> BigInt {
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
    floor_div(&(mid_fee * &reduction + out_fee * (&*WAD - &reduction)), &*WAD)
}

fn crypto_fee2(xp: &[BigInt], mid_fee: &BigInt, out_fee: &BigInt, fee_gamma: &BigInt) -> BigInt {
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
    floor_div(&(mid_fee * &reduction + out_fee * (&*WAD - &reduction)), &*WAD)
}

fn scaled_pair(
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

fn finish_pair(
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
    if !scale.is_positive() || dx.is_negative() || (i != 0 && i != 1) || (j != 0 && j != 1) || i == j
    {
        return None;
    }
    let xp = scaled_pair(balances, precs, scale, i, dx)?;
    let y = flat_y(amp, &xp, d, j)?;
    finish_pair(xp, y, j, scale, precs, mid_fee, out_fee, fee_gamma, flat_fee_on)
}

pub(crate) fn crypto2_out(
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
    if !scale.is_positive() || dx.is_negative() || (i != 0 && i != 1) || (j != 0 && j != 1) || i == j
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
    if !ann.is_positive() || !gamma.is_positive() || !d.is_positive() || !(0..=2).contains(&i) || x.len() != 3
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
        let y_plus = floor_div(&(&yfprime + &*WAD * d), &fprime)
            + floor_div(&(&y_minus * &*WAD), &k0);
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

fn crypto_fee3(xp: &[BigInt], mid_fee: &BigInt, out_fee: &BigInt, fee_gamma: &BigInt) -> BigInt {
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

pub(crate) fn crypto3_out(
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

pub(crate) fn sqrt_at_tick(tick: &BigInt) -> Option<BigInt> {
    let abs_tick = tick.abs();
    if abs_tick > *MAX_TICK {
        return None;
    }
    let mut ratio = if (&abs_tick & BigInt::one()).is_zero() {
        BigInt::one() << 128u32
    } else {
        TICK_BITS[0].clone()
    };
    for bit in 1..20 {
        if !(&abs_tick & (BigInt::one() << (bit as u32))).is_zero() {
            ratio = (&ratio * &TICK_BITS[bit as usize]) >> 128u32;
        }
    }
    if tick.is_positive() {
        ratio = floor_div(&((BigInt::one() << 256u32) - 1), &ratio);
    }
    let mut rounded = &ratio >> 32u32;
    if !(&ratio & ((BigInt::one() << 32u32) - BigInt::one())).is_zero() {
        rounded += 1;
    }
    Some(rounded)
}

fn ordered_prices(left: &BigInt, right: &BigInt) -> (BigInt, BigInt) {
    if left < right {
        (left.clone(), right.clone())
    } else {
        (right.clone(), left.clone())
    }
}

fn amount0_delta(sqrt_a: &BigInt, sqrt_b: &BigInt, liquidity: &BigInt, round_up: bool) -> Option<BigInt> {
    let (lo, hi) = ordered_prices(sqrt_a, sqrt_b);
    if !lo.is_positive() {
        return None;
    }
    let numerator1 = liquidity << 96u32;
    let numerator2 = &hi - &lo;
    if round_up {
        let inner = mul_div_up(&numerator1, &numerator2, &hi)?;
        return div_up(&inner, &lo);
    }
    Some(floor_div(&floor_div(&(&numerator1 * &numerator2), &hi), &lo))
}

fn amount1_delta(sqrt_a: &BigInt, sqrt_b: &BigInt, liquidity: &BigInt, round_up: bool) -> Option<BigInt> {
    let (lo, hi) = ordered_prices(sqrt_a, sqrt_b);
    let diff = &hi - &lo;
    if round_up {
        mul_div_up(liquidity, &diff, &Q96)
    } else {
        Some(floor_div(&(liquidity * &diff), &Q96))
    }
}

fn next_sqrt_from_amount0(sqrt: &BigInt, liquidity: &BigInt, amount: &BigInt) -> Option<BigInt> {
    if amount.is_zero() {
        return Some(sqrt.clone());
    }
    let numerator1 = liquidity << 96u32;
    let denom = &numerator1 + amount * sqrt;
    mul_div_up(&numerator1, sqrt, &denom)
}

fn next_sqrt_from_amount1(sqrt: &BigInt, liquidity: &BigInt, amount: &BigInt) -> Option<BigInt> {
    if amount.is_zero() {
        return Some(sqrt.clone());
    }
    if !liquidity.is_positive() {
        return None;
    }
    Some(sqrt + floor_div(&(amount * &*Q96), liquidity))
}

fn apply_liquidity(liquidity: &BigInt, delta: &BigInt) -> Option<BigInt> {
    if delta.is_negative() {
        if liquidity < &-delta {
            return None;
        }
        return Some(liquidity + delta);
    }
    Some(liquidity + delta)
}

fn bisect_right(keys: &[BigInt], target: &BigInt) -> usize {
    let mut lo = 0;
    let mut hi = keys.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if &keys[mid] <= target {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

fn next_initialized(
    keys: &[BigInt],
    current: &BigInt,
    include: bool,
    zero_for_one: bool,
) -> Option<BigInt> {
    if zero_for_one {
        let probe = if include {
            current.clone()
        } else {
            current - 1
        };
        let index = bisect_right(keys, &probe);
        if index == 0 {
            return None;
        }
        Some(keys[index - 1].clone())
    } else {
        let index = bisect_right(keys, current);
        if index >= keys.len() {
            return None;
        }
        Some(keys[index].clone())
    }
}

fn tick_net(nets: &BTreeMap<BigInt, BigInt>, boundary: &BigInt, zero_for_one: bool) -> Option<BigInt> {
    let net = nets.get(boundary)?.clone();
    if zero_for_one {
        Some(-net)
    } else {
        Some(net)
    }
}

pub(crate) fn concentrated_out(
    sqrt_price: &BigInt,
    liquidity: &BigInt,
    tick_start: &BigInt,
    fee_pips: &BigInt,
    ticks: &[(BigInt, BigInt)],
    zero_for_one: bool,
    amount: &BigInt,
) -> Option<BigInt> {
    let million = BigInt::from(1_000_000u32);
    if !sqrt_price.is_positive() || fee_pips.is_negative() || fee_pips > &million || amount.is_negative()
    {
        return None;
    }
    let mut ordered = ticks.to_vec();
    ordered.sort();
    let keys: Vec<BigInt> = ordered.iter().map(|(tick, _)| tick.clone()).collect();
    let mut nets = BTreeMap::new();
    for (tick, net) in &ordered {
        nets.insert(tick.clone(), net.clone());
    }
    let mut sqrt = sqrt_price.clone();
    let mut liq = liquidity.clone();
    let mut tick = tick_start.clone();
    let mut include = zero_for_one;
    let mut remaining = amount.clone();
    let mut amount_out = BigInt::zero();
    for _ in 0..ordered.len() + 2 {
        if !remaining.is_positive() {
            break;
        }
        let boundary = next_initialized(&keys, &tick, include, zero_for_one);
        include = false;
        let target = if let Some(bound) = &boundary {
            sqrt_at_tick(bound)?
        } else if zero_for_one {
            MIN_SQRT.clone()
        } else {
            MAX_SQRT.clone()
        };
        let on_tick = boundary.is_some() && target == sqrt;
        if !on_tick && ((zero_for_one && target >= sqrt) || (!zero_for_one && target <= sqrt)) {
            break;
        }
        if on_tick {
            let bound = boundary.clone()?;
            let net = tick_net(&nets, &bound, zero_for_one)?;
            liq = apply_liquidity(&liq, &net)?;
            tick = bound;
            continue;
        }
        if liq.is_zero() {
            let Some(bound) = boundary.clone() else {
                break;
            };
            let net = tick_net(&nets, &bound, zero_for_one)?;
            liq = apply_liquidity(&liq, &net)?;
            sqrt = target;
            tick = bound;
            continue;
        }
        let (in_to, out_full) = if zero_for_one {
            (
                amount0_delta(&target, &sqrt, &liq, true)?,
                amount1_delta(&target, &sqrt, &liq, false)?,
            )
        } else {
            (
                amount1_delta(&sqrt, &target, &liq, true)?,
                amount0_delta(&sqrt, &target, &liq, false)?,
            )
        };
        let less_fee = floor_div(&(&remaining * (&million - fee_pips)), &million);
        let reached;
        let step_in;
        let step_fee_paid;
        let step_out;
        let sqrt_next;
        if less_fee >= in_to {
            let step_fee = if fee_pips.is_zero() {
                Some(BigInt::zero())
            } else {
                mul_div_up(&in_to, fee_pips, &(&million - fee_pips))
            };
            if let Some(step_fee) = step_fee {
                if &in_to + &step_fee <= remaining {
                    reached = true;
                    step_in = in_to;
                    step_fee_paid = step_fee;
                    step_out = out_full;
                    sqrt_next = target;
                } else {
                    reached = false;
                    step_in = less_fee.clone();
                    step_fee_paid = &remaining - &less_fee;
                    sqrt_next = if zero_for_one {
                        next_sqrt_from_amount0(&sqrt, &liq, &less_fee)?
                    } else {
                        next_sqrt_from_amount1(&sqrt, &liq, &less_fee)?
                    };
                    step_out = if zero_for_one {
                        amount1_delta(&sqrt_next, &sqrt, &liq, false)?
                    } else {
                        amount0_delta(&sqrt, &sqrt_next, &liq, false)?
                    };
                }
            } else {
                reached = false;
                step_in = less_fee.clone();
                step_fee_paid = &remaining - &less_fee;
                sqrt_next = if zero_for_one {
                    next_sqrt_from_amount0(&sqrt, &liq, &less_fee)?
                } else {
                    next_sqrt_from_amount1(&sqrt, &liq, &less_fee)?
                };
                step_out = if zero_for_one {
                    amount1_delta(&sqrt_next, &sqrt, &liq, false)?
                } else {
                    amount0_delta(&sqrt, &sqrt_next, &liq, false)?
                };
            }
        } else {
            reached = false;
            step_in = less_fee.clone();
            step_fee_paid = &remaining - &less_fee;
            sqrt_next = if zero_for_one {
                next_sqrt_from_amount0(&sqrt, &liq, &less_fee)?
            } else {
                next_sqrt_from_amount1(&sqrt, &liq, &less_fee)?
            };
            step_out = if zero_for_one {
                amount1_delta(&sqrt_next, &sqrt, &liq, false)?
            } else {
                amount0_delta(&sqrt, &sqrt_next, &liq, false)?
            };
        }
        if &step_in + &step_fee_paid > remaining {
            return None;
        }
        remaining -= &step_in + &step_fee_paid;
        amount_out += &step_out;
        sqrt = sqrt_next;
        if boundary.is_none() || !reached {
            break;
        }
        let bound = boundary?;
        let net = tick_net(&nets, &bound, zero_for_one)?;
        liq = apply_liquidity(&liq, &net)?;
        tick = bound;
    }
    Some(amount_out)
}

pub(crate) fn bin_out(
    reference_x64: &BigInt,
    distance_e6: &BigInt,
    length_e6: &BigInt,
    position: &BigInt,
    amount: &BigInt,
    selling_token0: bool,
    scaled_out: &BigInt,
    scale_out: &BigInt,
) -> Option<BigInt> {
    if !reference_x64.is_positive() || amount.is_negative() {
        return None;
    }
    let million = BigInt::from(1_000_000u32);
    let lower = if distance_e6 >= &BigInt::zero() {
        floor_div(&(reference_x64 * (&million + distance_e6)), &million)
    } else {
        let distance = -distance_e6;
        if distance >= million {
            return None;
        }
        floor_div(&(reference_x64 * (&million - &distance)), &million)
    };
    let upper_distance = distance_e6 + length_e6;
    let upper = if upper_distance >= BigInt::zero() {
        floor_div(&(reference_x64 * (&million + &upper_distance)), &million)
    } else {
        let distance = -&upper_distance;
        if distance >= million {
            return None;
        }
        floor_div(&(reference_x64 * (&million - &distance)), &million)
    };
    if upper < lower {
        return None;
    }
    let span = &upper - &lower;
    let max_pos = (BigInt::one() << 104u32) - 1;
    let price = if span.is_zero() || !position.is_positive() {
        lower.clone()
    } else {
        &lower + floor_div(&(&span * position), &max_pos)
    };
    let mut out = if selling_token0 {
        floor_div(&(amount * &price), &(BigInt::one() << 64u32))
    } else if !price.is_positive() {
        return None;
    } else {
        floor_div(&(amount * (BigInt::one() << 64u32)), &price)
    };
    if scale_out.is_positive() && scaled_out.is_positive() {
        let available = floor_div(scaled_out, scale_out);
        if out > available {
            out = available;
        }
    }
    Some(out)
}

pub(crate) fn worst_bps(
    predict: &dyn Fn(BigInt) -> Option<BigInt>,
    rows: &[(BigInt, BigInt)],
) -> Option<i64> {
    let mut best: Option<i64> = None;
    for (amount, label) in rows {
        if !label.is_positive() {
            return None;
        }
        let pred = predict(amount.clone())?;
        if pred.is_negative() {
            return None;
        }
        let gap = (&pred - label).abs();
        if gap <= *DUST {
            continue;
        }
        let score = floor_div(&(&gap * 10_000), label).to_i64().unwrap_or(i64::MAX);
        best = Some(match best {
            Some(so_far) => so_far.max(score),
            None => score,
        });
    }
    Some(best.unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ib(n: i128) -> BigInt {
        BigInt::from(n)
    }

    fn s(text: &str) -> BigInt {
        parse_dec(text)
    }

    fn pair(a: i128, b: i128) -> (BigInt, BigInt) {
        (ib(a), ib(b))
    }

    #[test]
    fn self_check_and_inverses() {
        assert_eq!(
            product_out(&ib(1_000), &ib(1_000), &ib(1_000), &ib(3_000), &ib(1_000_000)),
            Some(ib(499))
        );
        assert_eq!(sqrt_at_tick(&BigInt::zero()), Some(Q96.clone()));
        let tick_one = sqrt_at_tick(&BigInt::one()).expect("tick 1");
        assert_eq!(tick_one, s("79232123823359799118286999568"));
        let balanced = vec![pow10(18), pow10(18)];
        let d = stable_d(&balanced, &ib(100)).expect("d");
        assert!((d - BigInt::from(2) * pow10(18)).abs() <= BigInt::from(2));
        assert_eq!(
            hyperbola_out(&ib(2), &ib(3), &ib(6), &ib(6), &ib(3)),
            Some(ib(4))
        );
        assert_eq!(hyperbola_in(&ib(2), &ib(3), &ib(6), &ib(6), &ib(4)), Some(ib(3)));
        let knots = [pair(100, 1100), pair(400, 1400)];
        let line = chord_quote(&knots).expect("line");
        assert_eq!(line(ib(200)), Some(ib(1200)));
        assert_eq!(line(ib(100)), Some(ib(1100)));
        assert_eq!(line(ib(400)), Some(ib(1400)));
        let backward = chord_in(&knots).expect("chord in");
        assert_eq!(backward(ib(1200)), Some(ib(200)));
        let curve = cubic_quote(&[pair(100, 1100), pair(400, 1400), pair(800, 1500)]).expect("curve");
        assert_eq!(curve(ib(100)), Some(ib(1100)));
        assert_eq!(curve(ib(400)), Some(ib(1400)));
        assert_eq!(curve(ib(800)), Some(ib(1500)));
        let repaired = residual_quote(&[pair(2, 3), pair(3, 5), pair(6, 6)]).expect("repaired");
        assert_eq!(repaired(ib(2)), Some(ib(3)));
        assert_eq!(repaired(ib(3)), Some(ib(5)));
        assert_eq!(repaired(ib(6)), Some(ib(6)));
        let base: Predict = Arc::new(Some);
        let lifted = added_residual(base, &[pair(100, 600), pair(400, 900)], &BigRational::one())
            .expect("lifted");
        assert_eq!(lifted(ib(100)), Some(ib(600)));
        assert_eq!(lifted(ib(200)), Some(ib(700)));
        assert_eq!(lifted(ib(400)), Some(ib(900)));
        let ann = ib(400_000);
        let gamma = s("145000000000000");
        let d_crypto = s("863257940499748221795");
        let balances = [s("535404253762953867722"), s("17320665696279630959")];
        let scale = crypto_scale(&ann, &gamma, &d_crypto, &balances[0], &balances[1], &ib(1), &ib(1))
            .expect("scale");
        let pred = crypto2_out(
            &balances,
            &[ib(1), ib(1)],
            &scale,
            &ann,
            &gamma,
            &d_crypto,
            0,
            1,
            &s("53540425376295386"),
            &ib(26_000_000),
            &ib(45_000_000),
            &s("230000000000000"),
        )
        .expect("crypto");
        let label = s("1738837333048969");
        assert!((&pred - &label).abs() <= ib(100));
        assert_eq!(
            flat_y(
                &ib(600_000),
                &[s("412253191892625532828380"), s("471799111780361957599989")],
                &s("884018281329555456735930"),
                1,
            ),
            Some(s("471798109549896825012548"))
        );
        assert_eq!(dynamic_fee(&ib(1), &ib(1), &ib(10), &ib(500), &ib(100)), ib(10));
        assert_eq!(dynamic_fee(&ib(1), &ib(100), &ib(10), &ib(500), &ib(100)), ib(43));
        // weighted_out(1000, 1000, 2, 1, 100, 0)
        assert_eq!(
            weighted_out(&ib(1000), &ib(1000), &ib(2), &ib(1), &ib(100), &ib(0)),
            Some(ib(173))
        );
        // weighted_out(10**18, 2*10**18, 3, 2, 10**17, 0)
        assert_eq!(
            weighted_out(&pow10(18), &(ib(2) * pow10(18)), &ib(3), &ib(2), &pow10(17), &ib(0)),
            Some(s("266431655917104881"))
        );
    }
}


