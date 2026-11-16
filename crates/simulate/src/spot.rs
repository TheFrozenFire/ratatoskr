//! Price of one raw unit from the first slope of a pair conduit.

use num_bigint::BigInt;

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
}
