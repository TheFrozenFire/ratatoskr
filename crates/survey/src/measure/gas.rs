//! Gas units charged the first time an action opens.

use std::collections::{BTreeMap, BTreeSet};

use action::Action;
use num_bigint::BigInt;
use searcher::{RouteSet, Token, NATIVE};

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
    let mut gas = BTreeMap::new();
    let mut kinds = BTreeMap::<u8, u64>::new();
    for edge in &set.edges {
        match &edge.curve {
            searcher::Curve::Wrap => {
                if let Some(action) = wrap_action(trade, edge.token_in, edge.token_out) {
                    let units = gas_units(&action);
                    gas.insert(action, units);
                }
            }
            searcher::Curve::Conduit(segments) => {
                let mut seen = BTreeSet::new();
                for segment in segments {
                    if segment.width <= BigInt::from(0) || segment.output <= BigInt::from(0) {
                        continue;
                    }
                    meta.segments += 1;
                    *kinds.entry(segment.action.kind).or_default() += 1;
                    if !seen.insert(segment.action) {
                        continue;
                    }
                    meta.unique += 1;
                    let action = reader::catalog_swap_cost(
                        source,
                        edge.token_in,
                        edge.token_out,
                        &segment.action,
                        cost,
                    )?;
                    let units = gas_units(&action);
                    gas.insert(action, units);
                }
            }
        }
    }
    meta.kinds = kinds
        .iter()
        .map(|(kind, count)| format!("{kind}:{count}"))
        .collect::<Vec<_>>()
        .join(",");
    Ok(gas)
}

fn wrap_action(trade: &trade::Trade, token_in: Token, token_out: Token) -> Option<Action> {
    let wrapped = trade.wrapped?;
    if wrapped == NATIVE {
        return None;
    }
    if token_in == NATIVE && token_out == wrapped {
        action::Wrap::new(wrapped, NATIVE).ok().map(Action::Wrap)
    } else if token_out == NATIVE && token_in == wrapped {
        action::Unwrap::new(wrapped, NATIVE)
            .ok()
            .map(Action::Unwrap)
    } else {
        None
    }
}

fn gas_units(action: &Action) -> BigInt {
    let units: i64 = match action {
        Action::Wrap(_) => 45_000,
        Action::Unwrap(_) => 35_000,
        Action::RateWrap(_) | Action::RateUnwrap(_) => 60_000,
        Action::Swap(swap) => match swap.invariant {
            action::Invariant::Product => 110_000,
            action::Invariant::Concentrated => 180_000,
            action::Invariant::Stable => 250_000,
            action::Invariant::Weighted => 200_000,
            action::Invariant::Crypto => 280_000,
            action::Invariant::Bin => 160_000,
            action::Invariant::Pmm => 150_000,
        },
        Action::MintLiquidity(_) | Action::BurnLiquidity(_) => 200_000,
        Action::DepositVault(_) | Action::RedeemVault(_) => 80_000,
        Action::Supply(_) | Action::Withdraw(_) => 120_000,
        Action::Stake(_) | Action::Unstake(_) => 120_000,
        Action::Convert(_) => 80_000,
        Action::Fill(_) => 150_000,
        Action::Split(_) | Action::Merge(_) => 100_000,
    };
    BigInt::from(units)
}
