//! The numeraire value of a finished plan, and the pools a measurement keeps.
//!
//! A price is the numeraire value of one raw unit. It is the first slope of
//! the pair conduit into the numeraire, or the best product through one hub
//! when that conduit is absent. [`score`] applies those prices to the amounts
//! the plan spent and the amounts it received. The gas is copied onto the
//! score. This crate does not search, does not allocate, and does not open
//! the catalog. The trading-set ledger quotes one direction from the reserves
//! it was opened on.

pub mod depletion;
mod spot;

pub use spot::spot_price;

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

/// Numeraire value of `units` of gas.
///
/// One gas unit costs `wei_per_gas` raw numeraire units. Native ETH is one raw
/// unit of the numeraire per wei, so the product is the gas cost. The allocator
/// does not read this figure.
#[must_use]
pub fn gas_value(units: &BigInt, wei_per_gas: u64) -> Ratio {
    if units.is_zero() || wei_per_gas == 0 {
        return Ratio::zero();
    }
    Ratio::new(units * BigInt::from(wei_per_gas), BigInt::from(1))
}

/// `value_out` minus the gas value.
#[must_use]
pub fn cumulative(value_out: &Ratio, gas: &Ratio) -> Ratio {
    value_out.sub(gas)
}

/// Share of input value the output does not return: `1 - retained`.
#[must_use]
pub fn impact(retained: &Ratio) -> Ratio {
    Ratio::new(BigInt::from(1), BigInt::from(1)).sub(retained)
}

/// Source value, sink value, and the spot of every token on one route.
///
/// The loss is the source spot times the input, minus the sink spot times the
/// output. An unpriced end leaves the loss empty. A token in the middle is
/// listed with its spot and does not change that difference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkedRoute {
    pub value_in: Option<Ratio>,
    pub value_out: Option<Ratio>,
    pub loss: Option<Ratio>,
    pub spots: Vec<(Token, Option<Price>)>,
}

#[must_use]
pub fn mark_route(
    path: &[Token],
    input: &BigInt,
    output: &BigInt,
    prices: &BTreeMap<Token, Price>,
) -> MarkedRoute {
    let spots = path
        .iter()
        .map(|token| (*token, quoted(*token, prices)))
        .collect();
    let value_in = path
        .first()
        .and_then(|token| priced_amount(*token, input, prices));
    let value_out = path
        .last()
        .and_then(|token| priced_amount(*token, output, prices));
    let loss = match (&value_in, &value_out) {
        (Some(left), Some(right)) => Some(left.sub(right)),
        _ => None,
    };
    MarkedRoute {
        value_in,
        value_out,
        loss,
        spots,
    }
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
    match priced_amount(leg.token, &leg.amount, prices) {
        Some(value) if value.numer.is_zero() => LegValue::Zero,
        Some(value) => LegValue::Priced(value),
        None if leg.amount.is_zero() => LegValue::Zero,
        None => LegValue::Missing,
    }
}

fn priced_amount(token: Token, amount: &BigInt, prices: &BTreeMap<Token, Price>) -> Option<Ratio> {
    if amount.is_zero() {
        return Some(Ratio::zero());
    }
    if amount.is_negative() {
        return None;
    }
    let price = quoted(token, prices)?;
    Some(Ratio::new(amount * &price.numer, price.denom.clone()))
}

fn quoted(token: Token, prices: &BTreeMap<Token, Price>) -> Option<Price> {
    let price = prices.get(&token)?;
    if price.denom <= BigInt::from(0) || price.numer.is_negative() {
        return None;
    }
    Some(price.clone())
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
    fn gas_value_is_units_times_wei_and_cumulative_is_output_minus_that_cost() {
        let mut prices = BTreeMap::new();
        prices.insert(token(1), price(1, 1));
        prices.insert(token(2), price(1, 1));
        let scored = score(
            &[leg(1, 4_000_000_000_000_000_000)],
            &[leg(2, 2_000_000_000_000_000_000)],
            &prices,
            &BigInt::from(210_000),
        );
        let gas = gas_value(&scored.gas, 300_000_000);
        let net = cumulative(&scored.value_out, &gas);
        assert_eq!(whole_tokens(&scored.value_out, 18, 6), "2.000000");
        assert_eq!(whole_tokens(&gas, 18, 6), "0.000063");
        assert_eq!(whole_tokens(&net, 18, 6), "1.999937");
        assert_eq!(
            scored.retained,
            Some(Ratio::new(BigInt::from(1), BigInt::from(2)))
        );
        assert_eq!(
            impact(scored.retained.as_ref().expect("retained")),
            Ratio::new(BigInt::from(1), BigInt::from(2))
        );
        let untouched = score(
            &[leg(1, 4_000_000_000_000_000_000)],
            &[leg(2, 2_000_000_000_000_000_000)],
            &prices,
            &BigInt::from(0),
        );
        assert_eq!(untouched.retained, scored.retained);
        assert_eq!(untouched.value_out, scored.value_out);
    }

    #[test]
    fn a_route_loss_is_the_source_spot_times_input_minus_the_sink_spot_times_output() {
        let mut prices = BTreeMap::new();
        prices.insert(token(1), price(2, 1));
        prices.insert(token(3), price(1, 1));
        let marked = mark_route(
            &[token(1), token(2), token(3)],
            &BigInt::from(10),
            &BigInt::from(9),
            &prices,
        );
        assert_eq!(
            marked.value_in,
            Some(Ratio::new(BigInt::from(20), BigInt::from(1)))
        );
        assert_eq!(
            marked.value_out,
            Some(Ratio::new(BigInt::from(9), BigInt::from(1)))
        );
        assert_eq!(
            marked.loss,
            Some(Ratio::new(BigInt::from(11), BigInt::from(1)))
        );
        assert_eq!(
            marked.spots,
            vec![
                (token(1), Some(price(2, 1))),
                (token(2), None),
                (token(3), Some(price(1, 1))),
            ]
        );
    }

    #[test]
    fn a_route_that_delivers_more_value_than_it_spends_has_a_negative_loss() {
        let mut prices = BTreeMap::new();
        prices.insert(token(1), price(1, 1));
        prices.insert(token(2), price(4, 1));
        let marked = mark_route(
            &[token(1), token(2)],
            &BigInt::from(5),
            &BigInt::from(5),
            &prices,
        );
        assert_eq!(
            marked.loss,
            Some(Ratio::new(BigInt::from(-15), BigInt::from(1)))
        );
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
