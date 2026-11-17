//! Price of one raw unit from the first slope of a pair conduit, and the
//! numeraire a sale of a raw amount fetches along that conduit.

use num_bigint::BigInt;
use num_traits::Signed;

use crate::{Price, SlopeSegment, Token};

/// First positive slope from `token` into `numeraire`.
///
/// Native ETH and the numeraire itself are one. When the direct conduit is
/// absent, the price is the best product of the first slope into a hub and
/// the first slope from that hub into the numeraire. A hub that is native ETH
/// uses the one-for-one rate on that leg.
pub fn spot_price(
    token: Token,
    numeraire: Token,
    hubs: &[Token],
    mut conduit: impl FnMut(Token, Token) -> Option<Vec<SlopeSegment>>,
) -> Option<Price> {
    if token == numeraire || token == action::NATIVE {
        return Some(Price::one());
    }
    if let Some(price) = marginal(&mut conduit, token, numeraire) {
        return Some(price);
    }
    let mut best = None;
    for hub in hubs {
        if *hub == token || *hub == numeraire {
            continue;
        }
        let Some(to_hub) = marginal(&mut conduit, token, *hub) else {
            continue;
        };
        let to_num = if *hub == action::NATIVE {
            Price::one()
        } else {
            let Some(price) = marginal(&mut conduit, *hub, numeraire) else {
                continue;
            };
            price
        };
        let price = to_hub.mul(&to_num);
        if best.as_ref().map(|held| price.beats(held)).unwrap_or(true) {
            best = Some(price);
        }
    }
    best
}

/// Numeraire raw units fetched by selling `amount` of `token`.
///
/// The numeraire and native ETH return the amount. A direct conduit is quoted
/// in full, including a quote of zero, and a hub is not substituted for it.
/// Without that conduit the sale is the hub path that pays the most, and a
/// native hub counts one for one on the leg into the numeraire. Zero is zero.
/// A negative amount, or no path, is missing.
pub fn realize(
    token: Token,
    amount: &BigInt,
    numeraire: Token,
    hubs: &[Token],
    mut conduit: impl FnMut(Token, Token) -> Option<Vec<SlopeSegment>>,
) -> Option<BigInt> {
    if amount.is_negative() {
        return None;
    }
    if amount == &BigInt::from(0) || token == numeraire || token == action::NATIVE {
        return Some(amount.clone());
    }
    if let Some(segments) = conduit(token, numeraire) {
        return Some(routing::quote(&segments, amount));
    }
    let mut best = None;
    for hub in hubs {
        if *hub == token || *hub == numeraire {
            continue;
        }
        let Some(into_hub) = sold(&mut conduit, token, *hub, amount) else {
            continue;
        };
        let into_num = if *hub == action::NATIVE {
            into_hub
        } else {
            let Some(paid) = sold(&mut conduit, *hub, numeraire, &into_hub) else {
                continue;
            };
            paid
        };
        if best.as_ref().map(|held| &into_num > held).unwrap_or(true) {
            best = Some(into_num);
        }
    }
    best
}

fn sold(
    conduit: &mut impl FnMut(Token, Token) -> Option<Vec<SlopeSegment>>,
    from: Token,
    to: Token,
    amount: &BigInt,
) -> Option<BigInt> {
    let segments = conduit(from, to)?;
    Some(routing::quote(&segments, amount))
}

fn marginal(
    conduit: &mut impl FnMut(Token, Token) -> Option<Vec<SlopeSegment>>,
    from: Token,
    to: Token,
) -> Option<Price> {
    let segments = conduit(from, to)?;
    let first = segments.first()?;
    if first.width <= BigInt::from(0) || first.output <= BigInt::from(0) {
        return None;
    }
    Some(Price {
        numer: first.output.clone(),
        denom: first.width.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SlopeSegment;
    use routing::Action;
    use std::collections::HashMap;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn segment(width: i64, output: i64) -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(width),
            output: BigInt::from(output),
            action: Action {
                kind: 1,
                pool: [1; 32],
            },
        }
    }

    #[test]
    fn a_missing_direct_conduit_uses_the_steeper_hub() {
        let wrapped = token(1);
        let priced = token(2);
        let steep = token(3);
        let shallow = token(4);
        let mut curves = HashMap::new();
        curves.insert((priced, steep), vec![segment(2, 4)]);
        curves.insert((steep, wrapped), vec![segment(1, 3)]);
        curves.insert((priced, shallow), vec![segment(2, 2)]);
        curves.insert((shallow, wrapped), vec![segment(1, 1)]);
        let price = spot_price(priced, wrapped, &[steep, shallow], |from, to| {
            curves.get(&(from, to)).cloned()
        })
        .expect("price");
        assert_eq!(price.numer, BigInt::from(12));
        assert_eq!(price.denom, BigInt::from(2));
    }

    #[test]
    fn native_eth_is_one_raw_unit() {
        let price = spot_price(action::NATIVE, token(1), &[], |_, _| None).expect("price");
        assert_eq!(price, Price::one());
    }

    #[test]
    fn a_sale_of_the_traded_amount_walks_past_a_dust_chord() {
        let wrapped = token(1);
        let token = token(2);
        let mut curves = HashMap::new();
        curves.insert(
            (token, wrapped),
            vec![segment(1, 1_000), segment(1_000, 10)],
        );
        let sold = realize(token, &BigInt::from(1_001), wrapped, &[], |from, to| {
            curves.get(&(from, to)).cloned()
        })
        .expect("sale");
        assert_eq!(sold, BigInt::from(1_010));
    }

    #[test]
    fn a_sale_inside_the_first_chord_matches_that_chord() {
        let wrapped = token(1);
        let token = token(2);
        let mut curves = HashMap::new();
        curves.insert((token, wrapped), vec![segment(100, 50)]);
        let sold = realize(token, &BigInt::from(10), wrapped, &[], |from, to| {
            curves.get(&(from, to)).cloned()
        })
        .expect("sale");
        assert_eq!(sold, BigInt::from(5));
    }

    #[test]
    fn a_direct_conduit_is_quoted_ahead_of_a_richer_hub() {
        let wrapped = token(1);
        let sold_token = token(2);
        let hub = token(3);
        let mut curves = HashMap::new();
        curves.insert((sold_token, wrapped), vec![segment(10, 5)]);
        curves.insert((sold_token, hub), vec![segment(10, 100)]);
        curves.insert((hub, wrapped), vec![segment(100, 100)]);
        let sold = realize(
            sold_token,
            &BigInt::from(10),
            wrapped,
            &[hub],
            |from, to| curves.get(&(from, to)).cloned(),
        )
        .expect("sale");
        assert_eq!(sold, BigInt::from(5));
    }

    #[test]
    fn a_negative_amount_has_no_sale() {
        let sold = realize(token(2), &BigInt::from(-1), token(1), &[], |_, _| None);
        assert!(sold.is_none());
    }
}
