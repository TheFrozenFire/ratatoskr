//! Prices of the tokens a measured trade spends or demands.

use std::collections::{BTreeMap, BTreeSet};

use searcher::{Conduits, Token};
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
        if let Some(price) = simulate::spot_price(token, stage.wrapped, &stage.hubs, |from, to| {
            reader.conduit(from, to)
        }) {
            prices.insert(token, price);
        }
    }
    prices
}
