//! Gas units charged the first time an action opens.

use std::collections::{BTreeMap, BTreeSet};

use num_bigint::BigInt;

use crate::{Action, RouteSet, Token, Trade, TradeError, NATIVE};

/// Gas charged when this action first receives input.
pub fn units(action: &Action) -> BigInt {
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

/// Gas map for the actions a route set opens.
///
/// A non-positive slope segment is omitted. `name` turns a pool key into the
/// action that spends `token_in` and pays `token_out`. A wrap that is not
/// native ETH against the trade's wrapped token is omitted.
pub fn open_gas(
    trade: &Trade,
    set: &RouteSet,
    mut name: impl FnMut(Token, Token, &routing::Action) -> Result<Action, TradeError>,
) -> Result<BTreeMap<Action, BigInt>, TradeError> {
    let mut gas = BTreeMap::new();
    for edge in &set.edges {
        match &edge.curve {
            searcher::Curve::Wrap => {
                if let Some(action) = wrap_action(trade, edge.token_in, edge.token_out) {
                    let amount = units(&action);
                    gas.insert(action, amount);
                }
            }
            searcher::Curve::Conduit(segments) => {
                let mut seen = BTreeSet::new();
                for segment in segments {
                    if segment.width <= BigInt::from(0) || segment.output <= BigInt::from(0) {
                        continue;
                    }
                    if !seen.insert(segment.action) {
                        continue;
                    }
                    let action = name(edge.token_in, edge.token_out, &segment.action)?;
                    let amount = units(&action);
                    gas.insert(action, amount);
                }
            }
        }
    }
    Ok(gas)
}

fn wrap_action(trade: &Trade, token_in: Token, token_out: Token) -> Option<Action> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SlopeSegment, Token};
    use num_bigint::BigUint;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn segment(width: i64, output: i64, pool: u8) -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(width),
            output: BigInt::from(output),
            action: routing::Action {
                kind: 1,
                pool: [pool; 32],
            },
        }
    }

    fn name(
        token_in: Token,
        token_out: Token,
        pool: &routing::Action,
    ) -> Result<Action, TradeError> {
        let contract = action::Address::new([pool.pool[0]; 20])?;
        Ok(Action::Swap(action::Swap::new(
            contract,
            contract.pool_id(),
            token_in,
            token_out,
            action::Invariant::Product,
        )?))
    }

    #[test]
    fn a_product_swap_and_a_wrap_use_their_units() {
        let wrapped = token(1);
        let trade = Trade {
            inputs: vec![(NATIVE, BigInt::from(10))],
            outputs: vec![(token(2), BigInt::from(1))],
            wrapped: Some(wrapped),
            gas_ceiling: None,
        };
        let set = RouteSet {
            inputs: vec![(NATIVE, BigUint::from(10_u8))],
            outputs: vec![token(2)],
            edges: vec![
                searcher::Edge {
                    token_in: NATIVE,
                    token_out: wrapped,
                    curve: searcher::Curve::Wrap,
                },
                searcher::Edge {
                    token_in: wrapped,
                    token_out: token(2),
                    curve: searcher::Curve::Conduit(vec![
                        segment(4, 2, 7),
                        segment(0, 1, 8),
                        segment(4, 1, 7),
                    ]),
                },
            ],
            routes: vec![searcher::Route { edges: vec![0, 1] }],
        };
        let gas = open_gas(&trade, &set, name).expect("gas");
        assert_eq!(gas.len(), 2);
        let wrap = gas
            .iter()
            .find(|(action, _)| matches!(action, Action::Wrap(_)))
            .expect("wrap");
        assert_eq!(wrap.1, &BigInt::from(45_000));
        let swap = gas
            .iter()
            .find(|(action, _)| matches!(action, Action::Swap(_)))
            .expect("swap");
        assert_eq!(swap.1, &BigInt::from(110_000));
    }
}
