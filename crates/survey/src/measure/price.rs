//! First slope of a conduit into the wrapped token.

use std::collections::{BTreeMap, BTreeSet};

use num_bigint::BigInt;
use searcher::{Conduits, Token, NATIVE};
use simulate::Price;

use crate::reader::Reader;
use crate::source::Source;
use crate::stage::Stage;

pub(super) fn spots<S: Source>(reader: &Reader<'_, S>, stage: &Stage) -> BTreeMap<Token, Price> {
    let mut tokens = BTreeSet::new();
    for scenario in &stage.scenarios {
        for (token, _) in &scenario.trade.inputs {
            tokens.insert(*token);
        }
        for (token, _) in &scenario.trade.outputs {
            tokens.insert(*token);
        }
    }
    let mut prices = BTreeMap::new();
    for token in tokens {
        if let Some(price) = spot(reader, token, stage.wrapped, &stage.hubs) {
            prices.insert(token, price);
        }
    }
    prices
}

fn spot<S: Source>(
    reader: &Reader<'_, S>,
    token: Token,
    numeraire: Token,
    hubs: &[Token],
) -> Option<Price> {
    if token == numeraire || token == NATIVE {
        return Some(Price::one());
    }
    if let Some(price) = marginal(reader, token, numeraire) {
        return Some(price);
    }
    let mut best = None;
    for hub in hubs {
        if *hub == token || *hub == numeraire {
            continue;
        }
        let Some(to_hub) = marginal(reader, token, *hub) else {
            continue;
        };
        let to_num = if *hub == NATIVE {
            Price::one()
        } else {
            let Some(price) = marginal(reader, *hub, numeraire) else {
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

fn marginal<S: Source>(reader: &Reader<'_, S>, from: Token, to: Token) -> Option<Price> {
    let segments = reader.conduit(from, to)?;
    let first = segments.first()?;
    if first.width <= BigInt::from(0) || first.output <= BigInt::from(0) {
        return None;
    }
    Some(Price {
        numer: first.output.clone(),
        denom: first.width.clone(),
    })
}
