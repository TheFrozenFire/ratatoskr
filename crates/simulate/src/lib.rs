//! The numeraire value of a finished plan, and the pools a measurement keeps.
//!
//! A price is the numeraire value of one raw unit. [`score`] applies those
//! prices to the amounts the plan spent and the amounts it received. The gas
//! is copied onto the score. This crate does not search, does not allocate,
//! and does not open the catalog.

use std::collections::{BTreeMap, BTreeSet};

use num_bigint::BigInt;
use num_traits::{Signed, ToPrimitive, Zero};

pub use action::Token;
pub use routing::SlopeSegment;

/// Numeraire value of one raw unit, as `numer / denom`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Price {
    pub numer: BigInt,
    pub denom: BigInt,
}

impl Price {
    #[must_use]
    pub fn one() -> Self {
        Self {
            numer: BigInt::from(1),
            denom: BigInt::from(1),
        }
    }

    /// Product of two rates. A non-positive denominator yields a zero price.
    #[must_use]
    pub fn mul(&self, other: &Self) -> Self {
        if self.denom <= BigInt::from(0) || other.denom <= BigInt::from(0) {
            return Self {
                numer: BigInt::from(0),
                denom: BigInt::from(1),
            };
        }
        Self {
            numer: &self.numer * &other.numer,
            denom: &self.denom * &other.denom,
        }
    }

    /// This rate pays more numeraire per raw unit.
    #[must_use]
    pub fn beats(&self, other: &Self) -> bool {
        &self.numer * &other.denom > &other.numer * &self.denom
    }
}

/// A reduced ratio. The denominator is positive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ratio {
    pub numer: BigInt,
    pub denom: BigInt,
}

impl Ratio {
    #[must_use]
    pub fn new(numer: BigInt, denom: BigInt) -> Self {
        if denom.is_zero() || numer.is_zero() {
            return Self::zero();
        }
        let neg = numer.sign() != denom.sign();
        let mut numer = numer.abs();
        let mut denom = denom.abs();
        let divisor = gcd(numer.clone(), denom.clone());
        numer /= &divisor;
        denom /= &divisor;
        if neg {
            numer = -numer;
        }
        Self { numer, denom }
    }

    #[must_use]
    pub fn zero() -> Self {
        Self {
            numer: BigInt::from(0),
            denom: BigInt::from(1),
        }
    }

    #[must_use]
    pub fn is_positive(&self) -> bool {
        self.numer.is_positive()
    }

    #[must_use]
    pub fn add(&self, other: &Self) -> Self {
        Self::new(
            &self.numer * &other.denom + &other.numer * &self.denom,
            &self.denom * &other.denom,
        )
    }

    #[must_use]
    pub fn div(&self, other: &Self) -> Self {
        Self::new(&self.numer * &other.denom, &self.denom * &other.numer)
    }

    #[must_use]
    pub fn sub(&self, other: &Self) -> Self {
        Self::new(
            &self.numer * &other.denom - &other.numer * &self.denom,
            &self.denom * &other.denom,
        )
    }

    #[must_use]
    pub fn cmp_value(&self, other: &Self) -> std::cmp::Ordering {
        (&self.numer * &other.denom).cmp(&(&other.numer * &self.denom))
    }
}

/// One token amount on a finished plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leg {
    pub token: Token,
    pub amount: BigInt,
}

/// What the plan spent and received, marked in the numeraire, and the gas it opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Score {
    pub value_in: Ratio,
    pub value_out: Ratio,
    /// `value_out / value_in` when every positive leg has a price and the input value is positive.
    pub retained: Option<Ratio>,
    pub gas: BigInt,
    pub unpriced: Vec<Token>,
}

/// Mark `spent` and `received` with `prices`.
///
/// A positive amount with no price, or a price whose denominator is not positive,
/// leaves [`Score::retained`] empty and names the token in [`Score::unpriced`].
/// Gas is stored and does not change the values.
#[must_use]
pub fn score(
    spent: &[Leg],
    received: &[Leg],
    prices: &BTreeMap<Token, Price>,
    gas: &BigInt,
) -> Score {
    let mut unpriced = BTreeSet::new();
    let mut value_in = Ratio::zero();
    let mut value_out = Ratio::zero();
    let mut complete = true;
    for leg in spent {
        match leg_value(leg, prices) {
            LegValue::Zero => {}
            LegValue::Priced(value) => value_in = value_in.add(&value),
            LegValue::Missing => {
                complete = false;
                unpriced.insert(leg.token);
            }
        }
    }
    for leg in received {
        match leg_value(leg, prices) {
            LegValue::Zero => {}
            LegValue::Priced(value) => value_out = value_out.add(&value),
            LegValue::Missing => {
                complete = false;
                unpriced.insert(leg.token);
            }
        }
    }
    let retained = if complete && value_in.is_positive() {
        Some(value_out.div(&value_in))
    } else {
        None
    };
    Score {
        value_in,
        value_out,
        retained,
        gas: gas.clone(),
        unpriced: unpriced.into_iter().collect(),
    }
}

enum LegValue {
    Zero,
    Priced(Ratio),
    Missing,
}

fn leg_value(leg: &Leg, prices: &BTreeMap<Token, Price>) -> LegValue {
    if leg.amount.is_zero() {
        return LegValue::Zero;
    }
    if leg.amount.is_negative() {
        return LegValue::Missing;
    }
    let Some(price) = prices.get(&leg.token) else {
        return LegValue::Missing;
    };
    if price.denom <= BigInt::from(0) || price.numer.is_negative() {
        return LegValue::Missing;
    }
    LegValue::Priced(Ratio::new(&leg.amount * &price.numer, price.denom.clone()))
}

/// Whole numeraire tokens of `ratio`, with `places` digits after the point.
///
/// `decimals` is the numeraire's raw scale. `1` raw unit with 18 decimals prints
/// as a fraction of one whole token.
#[must_use]
pub fn whole_tokens(ratio: &Ratio, decimals: u32, places: u32) -> String {
    let scale = pow10(decimals);
    let denom = &ratio.denom * &scale;
    format_div(&ratio.numer, &denom, places)
}

/// Unitless ratio, with `places` digits after the point.
#[must_use]
pub fn decimal(ratio: &Ratio, places: u32) -> String {
    format_div(&ratio.numer, &ratio.denom, places)
}

/// Pools in merge order, at most `pools` of them, with every segment of each.
///
/// The stored conduit lists slope segments steepest first. The first time a
/// pool appears is that pool's steepest piece. Later pieces of a kept pool stay.
/// `pools == 0` keeps nothing.
#[must_use]
pub fn keep_pools(segments: &[SlopeSegment], pools: usize) -> Vec<SlopeSegment> {
    if pools == 0 {
        return Vec::new();
    }
    let mut kept = BTreeSet::new();
    for segment in segments {
        if kept.len() == pools {
            break;
        }
        kept.insert(segment.action);
    }
    segments
        .iter()
        .filter(|segment| kept.contains(&segment.action))
        .cloned()
        .collect()
}

fn format_div(numer: &BigInt, denom: &BigInt, places: u32) -> String {
    if denom.is_zero() {
        return "0".to_string();
    }
    let neg = numer.sign() != denom.sign() && !numer.is_zero();
    let numer = numer.abs();
    let denom = denom.abs();
    let whole = &numer / &denom;
    let mut rest = &numer % &denom;
    let ten = BigInt::from(10);
    let mut frac = String::new();
    for _ in 0..places {
        rest *= &ten;
        let digit = &rest / &denom;
        rest %= &denom;
        let text = digit.to_u32().unwrap_or(0);
        frac.push(char::from_digit(text, 10).unwrap_or('0'));
    }
    let body = if places == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{frac}")
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

fn pow10(exp: u32) -> BigInt {
    let mut value = BigInt::from(1);
    let ten = BigInt::from(10);
    for _ in 0..exp {
        value *= &ten;
    }
    value
}

fn gcd(mut left: BigInt, mut right: BigInt) -> BigInt {
    while !right.is_zero() {
        let next = &left % &right;
        left = right;
        right = next;
    }
    left
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use num_bigint::BigInt;

    use super::*;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn price(numer: i64, denom: i64) -> Price {
        Price {
            numer: BigInt::from(numer),
            denom: BigInt::from(denom),
        }
    }

    fn leg(byte: u8, amount: i64) -> Leg {
        Leg {
            token: token(byte),
            amount: BigInt::from(amount),
        }
    }

    fn segment(pool: u8) -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(1),
            output: BigInt::from(1),
            action: routing::Action {
                kind: 1,
                pool: [pool; 32],
            },
        }
    }

    #[test]
    fn retained_is_output_value_over_input_value() {
        let mut prices = BTreeMap::new();
        prices.insert(token(1), price(2, 1));
        prices.insert(token(2), price(1, 1));
        let scored = score(&[leg(1, 10)], &[leg(2, 9)], &prices, &BigInt::from(50_000));
        assert_eq!(
            scored.retained,
            Some(Ratio::new(BigInt::from(9), BigInt::from(20)))
        );
        assert_eq!(scored.gas, BigInt::from(50_000));
        assert!(scored.unpriced.is_empty());
        let again = score(&[leg(1, 10)], &[leg(2, 9)], &prices, &BigInt::from(1));
        assert_eq!(again.retained, scored.retained);
        assert_eq!(again.value_in, scored.value_in);
    }

    #[test]
    fn a_missing_price_leaves_retention_empty() {
        let mut prices = BTreeMap::new();
        prices.insert(token(1), price(1, 1));
        let scored = score(&[leg(1, 4)], &[leg(2, 4)], &prices, &BigInt::from(0));
        assert!(scored.retained.is_none());
        assert_eq!(scored.unpriced, vec![token(2)]);
    }

    #[test]
    fn whole_tokens_apply_the_numeraire_scale() {
        let ratio = Ratio::new(BigInt::from(1_500_000_000_000_000_000_i64), BigInt::from(1));
        assert_eq!(whole_tokens(&ratio, 18, 4), "1.5000");
    }

    #[test]
    fn keep_pools_follows_first_appearance() {
        let segments = vec![segment(1), segment(2), segment(1), segment(3)];
        let kept = keep_pools(&segments, 2);
        let pools: Vec<u8> = kept.iter().map(|segment| segment.action.pool[0]).collect();
        assert_eq!(pools, vec![1, 2, 1]);
        assert!(keep_pools(&segments, 0).is_empty());
    }
}
