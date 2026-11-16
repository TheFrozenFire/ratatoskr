//! Shared integer scale, logarithms, and the quote closure type.

use bigdecimal::{BigDecimal, Context, RoundingMode};
use num_bigint::{BigInt, Sign};
use num_integer::Integer;
use num_rational::BigRational;
use num_traits::{One, Signed, ToPrimitive, Zero};
use std::cmp::Ordering;
use std::num::NonZeroU64;
use std::sync::{Arc, LazyLock};

pub(crate) type Predict = Arc<dyn Fn(BigInt) -> Option<BigInt> + Send + Sync>;

pub(crate) static WAD: LazyLock<BigInt> = LazyLock::new(|| pow10(18));

pub(crate) static FEE_1E10: LazyLock<BigInt> = LazyLock::new(|| pow10(10));

pub(crate) static A_MULTIPLIER: LazyLock<BigInt> = LazyLock::new(|| BigInt::from(10_000));

pub(crate) static Q96: LazyLock<BigInt> = LazyLock::new(|| BigInt::one() << 96u32);

pub(crate) static MIN_SQRT: LazyLock<BigInt> = LazyLock::new(|| BigInt::from(4_295_128_739u64));

pub(crate) static MAX_SQRT: LazyLock<BigInt> =
    LazyLock::new(|| parse_dec("1461446703485210103287273052203988822378723970342"));

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
    HEX.iter().map(|text| parse_hex(text)).collect()
});

pub(crate) static LN2: LazyLock<BigRational> = LazyLock::new(ln2_series);

pub(crate) fn parse_dec(text: &str) -> BigInt {
    match BigInt::parse_bytes(text.as_bytes(), 10) {
        Some(value) => value,
        None => BigInt::zero(),
    }
}

pub(crate) fn parse_hex(text: &str) -> BigInt {
    match BigInt::parse_bytes(text.as_bytes(), 16) {
        Some(value) => value,
        None => BigInt::zero(),
    }
}

static POW10: LazyLock<Vec<BigInt>> = LazyLock::new(|| {
    (0..=256u32)
        .map(|exp| BigInt::from(10u32).pow(exp))
        .collect()
});

pub(crate) fn pow10(exp: u64) -> BigInt {
    // A quote scale past this is not a token amount. Capping keeps a wild
    // input from asking for 10^(2^32).
    let exp = u32::try_from(exp).unwrap_or(4_096).min(4_096);
    if exp <= 256 {
        POW10[exp as usize].clone()
    } else {
        BigInt::from(10u32).pow(exp)
    }
}

pub(crate) fn floor_div(numer: &BigInt, denom: &BigInt) -> BigInt {
    if denom.is_zero() {
        BigInt::zero()
    } else {
        numer.div_floor(denom)
    }
}

pub(crate) fn prec_80() -> NonZeroU64 {
    match NonZeroU64::new(80) {
        Some(value) => value,
        None => NonZeroU64::MIN,
    }
}

pub(crate) fn context_floor(value: BigDecimal) -> BigDecimal {
    Context::new(prec_80(), RoundingMode::Floor).round_decimal(value)
}

fn cmp_abs_pow10(n: &BigInt, exp: u64) -> Ordering {
    let Ok(exp) = u32::try_from(exp) else {
        return Ordering::Less;
    };
    let pow = if exp <= 4_096 {
        pow10(u64::from(exp))
    } else {
        BigInt::from(10u32).pow(exp)
    };
    match n.sign() {
        Sign::Minus => n.abs().cmp(&pow),
        _ => n.cmp(&pow),
    }
}

pub(crate) fn floor_log10_int(n: &BigInt) -> u64 {
    if n.is_zero() {
        return 0;
    }
    let bits = n.bits();
    let mut guess = ((bits - 1) as f64 * std::f64::consts::LOG10_2).floor() as u64;
    if guess >= bits {
        guess = bits - 1;
    }
    // The bit-length guess can be off. Step until 10^guess <= |n| < 10^(guess+1),
    // which is the digit count minus one.
    loop {
        if cmp_abs_pow10(n, guess) == Ordering::Less {
            if guess == 0 {
                return 0;
            }
            guess -= 1;
            continue;
        }
        if guess == u64::MAX || cmp_abs_pow10(n, guess + 1) == Ordering::Less {
            return guess;
        }
        guess += 1;
    }
}

pub(crate) fn cmp_pow10(numer: &BigInt, denom: &BigInt, exp: i64) -> Ordering {
    if exp >= 0 {
        numer.cmp(&(denom * pow10(exp as u64)))
    } else {
        (numer * pow10((-exp) as u64)).cmp(denom)
    }
}

pub(crate) fn floor_log10_ratio(numer: &BigInt, denom: &BigInt) -> i64 {
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

pub(crate) fn cmp_pow2(numer: &BigInt, denom: &BigInt, exp: i64) -> Ordering {
    if exp >= 0 {
        numer.cmp(&(denom * (BigInt::one() << exp as u32)))
    } else {
        (numer * (BigInt::one() << ((-exp) as u32))).cmp(denom)
    }
}

pub(crate) fn floor_log2_ratio(numer: &BigInt, denom: &BigInt) -> i64 {
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
pub(crate) fn round_sig_floor_dec(numer: &BigInt, denom: &BigInt, prec: u64) -> BigDecimal {
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

pub(crate) fn rat_from_decimal(value: &BigDecimal) -> BigRational {
    let (digits, scale) = value.as_bigint_and_exponent();
    if scale > 0 {
        BigRational::new(digits, pow10(scale as u64))
    } else if scale < 0 {
        BigRational::from_integer(digits * pow10((-scale) as u64))
    } else {
        BigRational::from_integer(digits)
    }
}

pub(crate) fn floor80_ratio(value: &BigRational) -> BigRational {
    if !value.numer().is_positive() {
        return BigRational::zero();
    }
    let dec = round_sig_floor_dec(value.numer(), value.denom(), 80);
    let (digits, _) = dec.as_bigint_and_exponent();
    // `round_sig_floor_dec` already keeps 80 significant digits. The decimal
    // context repeats that floor, and it is a no-op while the coefficient
    // stays below 10^80.
    if digits < pow10(80) {
        return rat_from_decimal(&dec);
    }
    rat_from_decimal(&context_floor(dec))
}

pub(crate) fn floor_rat(value: &BigRational) -> BigInt {
    value.numer().div_floor(value.denom())
}

pub(crate) fn trunc_rat(value: &BigRational) -> BigInt {
    value.numer() / value.denom()
}

pub(crate) fn ln2_series() -> BigRational {
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

pub(crate) fn ln_unit(y: &BigRational) -> BigRational {
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

pub(crate) fn ln_pos(x: &BigRational) -> BigRational {
    let k = floor_log2_ratio(x.numer(), x.denom());
    let shift = k + 1;
    let y = if shift >= 0 {
        x / BigRational::from_integer(BigInt::one() << shift as u32)
    } else {
        x * BigRational::from_integer(BigInt::one() << ((-shift) as u32))
    };
    ln_unit(&y) + BigRational::from_integer(BigInt::from(shift)) * LN2.clone()
}

pub(crate) fn ln_ten() -> BigRational {
    ln_unit(&BigRational::new(BigInt::from(5), BigInt::from(8)))
        + BigRational::from_integer(BigInt::from(4)) * LN2.clone()
}

pub(crate) fn exp_series(t: &BigRational) -> BigRational {
    let mut term = BigRational::one();
    let mut sum = BigRational::one();
    for n in 1..320 {
        term *= t;
        term /= BigRational::from_integer(BigInt::from(n));
        sum += &term;
    }
    sum
}

pub(crate) fn scale_int(digits: BigInt, exp: i64) -> BigRational {
    if exp >= 0 {
        BigRational::from_integer(digits * pow10(exp as u64))
    } else {
        BigRational::new(digits, pow10((-exp) as u64))
    }
}

pub(crate) fn exp_floor80(t: &BigRational) -> BigRational {
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

fn int_pow(base: &BigInt, mut exp: u32) -> BigInt {
    let mut result = BigInt::one();
    let mut value = base.clone();
    while exp > 0 {
        if exp & 1 == 1 {
            result *= &value;
        }
        exp >>= 1;
        if exp > 0 {
            value = &value * &value;
        }
    }
    result
}

pub(crate) fn small_u32(exp: &BigRational) -> Option<u32> {
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

pub(crate) fn bd_at(prec: u64) -> Context {
    Context::new(
        NonZeroU64::new(prec).expect("precision"),
        RoundingMode::Floor,
    )
}

pub(crate) fn bd_floor(value: BigDecimal, prec: u64) -> BigDecimal {
    if value.sign() == Sign::NoSign {
        return BigDecimal::from(0);
    }
    bd_at(prec).round_decimal(value)
}

pub(crate) fn ratio_bd(value: &BigRational, prec: u64) -> BigDecimal {
    round_sig_floor_dec(value.numer(), value.denom(), prec)
}

pub(crate) fn ln2_bd(prec: u64) -> BigDecimal {
    let text = "693147180559945309417232121458176568075500134360255254120680009493393621969694715605863326996418687542001481020570685733685520235758130557032670751635";
    let keep = prec.min(text.len() as u64) as usize;
    let digits = parse_dec(&text[..keep]);
    bd_floor(BigDecimal::new(digits, keep as i64), prec)
}

pub(crate) fn bd_div(numer: &BigDecimal, denom: &BigDecimal, prec: u64) -> BigDecimal {
    let ctx = bd_at(prec + 4);
    bd_floor(numer * denom.inverse_with_context(&ctx), prec)
}

pub(crate) fn ln_bd(value: &BigDecimal, prec: u64) -> Option<BigDecimal> {
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

pub(crate) fn decimal_pow(base: &BigRational, exp: &BigRational) -> BigRational {
    if !base.numer().is_positive() {
        return BigRational::zero();
    }
    if exp.is_zero() || base == &BigRational::one() {
        return BigRational::one();
    }
    if let Some(k) = small_u32(exp) {
        // The reduced power and the unreduced power are the same rational.
        // Skipping the intermediate gcd leaves one 80-digit floor.
        let numer = int_pow(base.numer(), k);
        let denom = int_pow(base.denom(), k);
        return floor80_ratio(&BigRational::new_raw(numer, denom));
    }
    if &(exp.numer() * 4) == exp.denom() {
        if let Some(root) = quarter_root(base) {
            return root;
        }
    }
    decimal_pow_series(base, exp)
}

fn decimal_pow_series(base: &BigRational, exp: &BigRational) -> BigRational {
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

/// `(numer/denom)^(1/4)` at the same 80-digit floor as the log series,
/// from two integer square roots at 120 guard digits.
fn quarter_root(base: &BigRational) -> Option<BigRational> {
    if !base.numer().is_positive() || !base.denom().is_positive() {
        return None;
    }
    const DIGITS: u64 = 120;
    let scale = pow10(DIGITS * 4);
    let scaled = (base.numer() * &scale).div_floor(base.denom());
    if !scaled.is_positive() {
        return Some(BigRational::zero());
    }
    let root = BigInt::from(scaled.magnitude().sqrt().sqrt());
    Some(floor80_ratio(&BigRational::new(root, pow10(DIGITS))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quarter_root_matches_the_log_series() {
        let quarter = BigRational::new(BigInt::from(1), BigInt::from(4));
        let mut bases = vec![
            BigRational::new(BigInt::from(1), BigInt::from(2)),
            BigRational::new(BigInt::from(2), BigInt::from(3)),
            BigRational::new(BigInt::from(3), BigInt::from(4)),
            BigRational::new(BigInt::from(999), BigInt::from(1000)),
            BigRational::new(BigInt::from(1000), BigInt::from(1001)),
            BigRational::new(BigInt::from(5), BigInt::from(2)),
            BigRational::new(BigInt::from(1), BigInt::from(10).pow(6)),
            BigRational::from_integer(BigInt::from(10).pow(6)),
        ];
        let wad = BigInt::from(10).pow(18);
        let balance_in = BigInt::from(1000u64) * &wad;
        let balance_out = BigInt::from(4000u64) * &wad;
        let fee = BigInt::from(3) * BigInt::from(10).pow(15);
        let amount = BigInt::from(10).pow(16);
        let after = floor_div(&(&amount * (&wad - &fee)), &wad);
        bases.push(floor80_ratio(&BigRational::new(
            balance_in.clone(),
            &balance_in + &after,
        )));
        bases.push(floor80_ratio(&BigRational::new(
            balance_out.clone(),
            &balance_out + &after,
        )));
        for base in bases {
            let root = quarter_root(&base).expect("root");
            assert_eq!(root, decimal_pow_series(&base, &quarter), "base {base}");
            assert_eq!(decimal_pow(&base, &quarter), root);
        }
    }
}
