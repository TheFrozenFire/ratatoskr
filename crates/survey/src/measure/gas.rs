//! Gas map for one measured trade, plus the segment counts the log prints.

use std::collections::{BTreeMap, BTreeSet};

use action::Action;
use num_bigint::BigInt;
use searcher::RouteSet;

use crate::reader::{self, NameCost};
use crate::source::Source;

#[derive(Default)]
pub(super) struct GasMeta {
    pub(super) segments: u64,
    pub(super) unique: u64,
    pub(super) kinds: String,
}

pub(super) fn book_gas<S: Source>(
    source: &S,
    trade: &trade::Trade,
    set: &RouteSet,
    cost: &mut NameCost,
    meta: &mut GasMeta,
) -> Result<BTreeMap<Action, BigInt>, trade::TradeError> {
    let _span = observability::info_span!("book_gas").entered();
    count_segments(set, meta);
    trade::open_gas(trade, set, |token_in, token_out, key| {
        reader::catalog_swap_cost(source, token_in, token_out, key, cost)
    })
}

fn count_segments(set: &RouteSet, meta: &mut GasMeta) {
    let mut kinds = BTreeMap::<u8, u64>::new();
    for edge in &set.edges {
        let searcher::Curve::Conduit(segments) = &edge.curve else {
            continue;
        };
        let mut seen = BTreeSet::new();
        for segment in segments {
            if segment.width <= BigInt::from(0) || segment.output <= BigInt::from(0) {
                continue;
            }
            meta.segments += 1;
            *kinds.entry(segment.action.kind).or_default() += 1;
            if seen.insert(segment.action) {
                meta.unique += 1;
            }
        }
    }
    meta.kinds = kinds
        .iter()
        .map(|(kind, count)| format!("{kind}:{count}"))
        .collect::<Vec<_>>()
        .join(",");
}
