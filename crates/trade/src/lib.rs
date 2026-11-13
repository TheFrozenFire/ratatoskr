//! The definition of one trade, and the problem built from its search.
//!
//! A trade names the tokens it spends, the amount of each, the tokens it
//! wants back, and the weight of each. It names the wrapped token when an
//! input is native ETH, and it can name a gas ceiling. [`query`] checks that
//! definition and hands it to the searcher. [`problem`] hands each pair
//! conduit to the allocator as one edge. The chords stay in the stored merge,
//! and each chord names the action that receives that input. [`allocate`]
//! returns the plan.
//!
//! The caller names each pool key as an [`Action`]. A custodial wrap is one
//! unlimited edge, built from native ETH on one side of the wrap curve. This
//! crate does not read a venue kind and does not open the catalog.

use std::collections::BTreeMap;
use std::fmt;

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use optimizer::{optimize, Chord, Curve, Edge, Input, Output, Route};

pub use action::{Action, Token, NATIVE};
pub use optimizer::{OptimizeError, Plan, Problem};
pub use routing::SlopeSegment;
pub use searcher::{Conduits, Query, RouteSet, SearchError};

/// Inputs, basket weights, the wrapped token, and an optional gas ceiling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trade {
    pub inputs: Vec<(Token, BigInt)>,
    pub outputs: Vec<(Token, BigInt)>,
    /// Wrapped token paired with native ETH. `None` leaves the wrap out.
    pub wrapped: Option<Token>,
    /// Largest gas the plan may open. `None` leaves it unbounded.
    pub gas_ceiling: Option<BigInt>,
}

/// A trade that cannot be searched or handed to the allocator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TradeError {
    Search(SearchError),
    Optimize(OptimizeError),
    Action(action::ActionError),
    Amount,
    Weight,
    Ceiling,
    Gas,
    Wrap,
    Curve,
    RouteSet,
}

impl fmt::Display for TradeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TradeError::Search(err) => write!(formatter, "{err}"),
            TradeError::Optimize(err) => write!(formatter, "{err}"),
            TradeError::Action(err) => write!(formatter, "{err}"),
            TradeError::Amount => formatter.write_str("an input amount is not positive"),
            TradeError::Weight => formatter.write_str("an output weight is not positive"),
            TradeError::Ceiling => formatter.write_str("a gas ceiling is negative"),
            TradeError::Gas => formatter.write_str("a gas amount is negative"),
            TradeError::Wrap => {
                formatter.write_str("a wrap is not between native ETH and its wrapped token")
            }
            TradeError::Curve => formatter.write_str("a curve is empty"),
            TradeError::RouteSet => formatter.write_str("the route set does not match the trade"),
        }
    }
}

impl std::error::Error for TradeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            TradeError::Search(err) => Some(err),
            TradeError::Optimize(err) => Some(err),
            TradeError::Action(err) => Some(err),
            _ => None,
        }
    }
}

impl From<SearchError> for TradeError {
    fn from(err: SearchError) -> Self {
        TradeError::Search(err)
    }
}

impl From<OptimizeError> for TradeError {
    fn from(err: OptimizeError) -> Self {
        TradeError::Optimize(err)
    }
}

impl From<action::ActionError> for TradeError {
    fn from(err: action::ActionError) -> Self {
        TradeError::Action(err)
    }
}

/// Check the trade and project it into the search.
pub fn query(trade: &Trade, hubs: &[Token]) -> Result<Query, TradeError> {
    check(trade)?;
    let mut inputs = Vec::with_capacity(trade.inputs.len());
    for (token, amount) in &trade.inputs {
        inputs.push((*token, amount_of(amount)?));
    }
    let outputs = trade.outputs.iter().map(|(token, _)| *token).collect();
    Ok(searcher::prepare(
        searcher::Trade { inputs, outputs },
        hubs,
        trade.wrapped,
    )?)
}

/// Hand each pair conduit to the allocator as one edge.
///
/// `name` turns a pool key into the action that spends `token_in` and pays
/// `token_out`. A chord keeps that action. Gas is copied from `gas`. A missing
/// key is left missing, and the allocator charges it as zero. The routes are
/// the searcher's indexes.
pub fn problem(
    trade: &Trade,
    hubs: &[Token],
    set: &RouteSet,
    gas: &BTreeMap<Action, BigInt>,
    mut name: impl FnMut(Token, Token, &routing::Action) -> Result<Action, TradeError>,
) -> Result<Problem, TradeError> {
    check(trade)?;
    let gas = booked(gas)?;
    if !same_trade(trade, set) {
        return Err(TradeError::RouteSet);
    }
    let mut edges = Vec::with_capacity(set.edges.len());
    for edge in &set.edges {
        edges.push(conduit_edge(trade, edge, &mut name)?);
    }
    let mut routes = Vec::with_capacity(set.routes.len());
    for route in &set.routes {
        for index in &route.edges {
            if *index >= set.edges.len() {
                return Err(TradeError::RouteSet);
            }
        }
        routes.push(Route {
            edges: route.edges.clone(),
        });
    }
    Ok(Problem {
        inputs: trade
            .inputs
            .iter()
            .map(|(token, amount)| Input {
                token: *token,
                amount: amount.clone(),
            })
            .collect(),
        outputs: trade
            .outputs
            .iter()
            .map(|(token, weight)| Output {
                token: *token,
                weight: weight.clone(),
            })
            .collect(),
        edges,
        routes,
        hubs: hubs.to_vec(),
        gas,
        gas_ceiling: trade.gas_ceiling.clone(),
    })
}

/// Search the conduits and allocate the trade.
pub fn allocate(
    trade: &Trade,
    hubs: &[Token],
    conduits: &impl Conduits,
    gas: &BTreeMap<Action, BigInt>,
    name: impl FnMut(Token, Token, &routing::Action) -> Result<Action, TradeError>,
) -> Result<Plan, TradeError> {
    let searched = query(trade, hubs)?;
    let set = searcher::execute(&searched, conduits);
    let problem = problem(trade, hubs, &set, gas, name)?;
    Ok(optimize(&problem)?)
}

fn check(trade: &Trade) -> Result<(), TradeError> {
    for (_, amount) in &trade.inputs {
        amount_of(amount)?;
    }
    for (_, weight) in &trade.outputs {
        if positive(weight).is_none() {
            return Err(TradeError::Weight);
        }
    }
    if let Some(ceiling) = &trade.gas_ceiling {
        if ceiling < &BigInt::from(0) {
            return Err(TradeError::Ceiling);
        }
    }
    Ok(())
}

fn amount_of(amount: &BigInt) -> Result<BigUint, TradeError> {
    positive(amount).ok_or(TradeError::Amount)
}

fn positive(amount: &BigInt) -> Option<BigUint> {
    amount.to_biguint().filter(|value| !value.is_zero())
}

fn booked(gas: &BTreeMap<Action, BigInt>) -> Result<BTreeMap<Action, BigInt>, TradeError> {
    let mut copy = BTreeMap::new();
    for (action, amount) in gas {
        if amount < &BigInt::from(0) {
            return Err(TradeError::Gas);
        }
        copy.insert(action.clone(), amount.clone());
    }
    Ok(copy)
}

fn same_trade(trade: &Trade, set: &RouteSet) -> bool {
    if trade.inputs.len() != set.inputs.len() || trade.outputs.len() != set.outputs.len() {
        return false;
    }
    for ((token, amount), (set_token, set_amount)) in trade.inputs.iter().zip(&set.inputs) {
        let Some(amount) = positive(amount) else {
            return false;
        };
        if token != set_token || amount != *set_amount {
            return false;
        }
    }
    trade
        .outputs
        .iter()
        .zip(&set.outputs)
        .all(|((token, _), set_token)| token == set_token)
}

fn conduit_edge(
    trade: &Trade,
    edge: &searcher::Edge,
    name: &mut impl FnMut(Token, Token, &routing::Action) -> Result<Action, TradeError>,
) -> Result<Edge, TradeError> {
    match &edge.curve {
        searcher::Curve::Wrap => Ok(Edge {
            action: custodial(trade, edge.token_in, edge.token_out)?,
            curve: Curve::Unlimited,
        }),
        searcher::Curve::Conduit(segments) => {
            let mut chords = Vec::new();
            for segment in segments {
                if segment.width <= BigInt::from(0) || segment.output <= BigInt::from(0) {
                    continue;
                }
                chords.push(Chord {
                    width: segment.width.clone(),
                    output: segment.output.clone(),
                    action: name(edge.token_in, edge.token_out, &segment.action)?,
                });
            }
            let Some(action) = chords.first().map(|chord| chord.action.clone()) else {
                return Err(TradeError::Curve);
            };
            Ok(Edge {
                action,
                curve: Curve::Chords(chords),
            })
        }
    }
}

fn custodial(trade: &Trade, token_in: Token, token_out: Token) -> Result<Action, TradeError> {
    let Some(wrapped) = trade.wrapped else {
        return Err(TradeError::Wrap);
    };
    if wrapped == NATIVE {
        return Err(TradeError::Wrap);
    }
    if token_in == NATIVE && token_out == wrapped {
        Ok(Action::Wrap(action::Wrap::new(wrapped, NATIVE)?))
    } else if token_out == NATIVE && token_in == wrapped {
        Ok(Action::Unwrap(action::Unwrap::new(wrapped, NATIVE)?))
    } else {
        Err(TradeError::Wrap)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use num_bigint::{BigInt, BigUint};

    use super::*;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn key(kind: u8, pool: u8) -> routing::Action {
        routing::Action {
            kind,
            pool: [pool; 32],
        }
    }

    fn segment(width: i64, output: i64, kind: u8, pool: u8) -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(width),
            output: BigInt::from(output),
            action: key(kind, pool),
        }
    }

    fn name_pool(
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

    fn sample() -> Trade {
        Trade {
            inputs: vec![(token(1), BigInt::from(10))],
            outputs: vec![(token(2), BigInt::from(1))],
            wrapped: None,
            gas_ceiling: None,
        }
    }

    fn conduit(token_in: Token, token_out: Token, segments: Vec<SlopeSegment>) -> searcher::Edge {
        searcher::Edge {
            token_in,
            token_out,
            curve: searcher::Curve::Conduit(segments),
        }
    }

    fn route_set(edges: Vec<searcher::Edge>, routes: Vec<Vec<usize>>) -> RouteSet {
        RouteSet {
            inputs: vec![(token(1), BigUint::from(10_u8))],
            outputs: vec![token(2)],
            edges,
            routes: routes
                .into_iter()
                .map(|edges| searcher::Route { edges })
                .collect(),
        }
    }

    struct Direct(Vec<SlopeSegment>);

    impl Conduits for Direct {
        fn conduit(&self, token_in: Token, token_out: Token) -> Option<Vec<SlopeSegment>> {
            if token_in == token(1) && token_out == token(2) {
                Some(self.0.clone())
            } else {
                None
            }
        }
    }

    #[test]
    fn a_non_positive_amount_is_rejected() {
        let mut trade = sample();
        trade.inputs[0].1 = BigInt::from(0);
        assert!(matches!(query(&trade, &[]), Err(TradeError::Amount)));
        trade.inputs[0].1 = BigInt::from(-1);
        assert!(matches!(query(&trade, &[]), Err(TradeError::Amount)));
    }

    #[test]
    fn a_non_positive_weight_is_rejected() {
        let mut trade = sample();
        trade.outputs[0].1 = BigInt::from(0);
        assert!(matches!(query(&trade, &[]), Err(TradeError::Weight)));
        trade.outputs[0].1 = BigInt::from(-2);
        assert!(matches!(query(&trade, &[]), Err(TradeError::Weight)));
    }

    #[test]
    fn a_negative_ceiling_is_rejected() {
        let mut trade = sample();
        trade.gas_ceiling = Some(BigInt::from(-1));
        assert!(matches!(query(&trade, &[]), Err(TradeError::Ceiling)));
    }

    #[test]
    fn a_native_wrapper_is_rejected() {
        let mut trade = sample();
        trade.wrapped = Some(NATIVE);
        assert!(matches!(
            query(&trade, &[]),
            Err(TradeError::Search(SearchError::WrappedIsNative))
        ));
    }

    #[test]
    fn an_overlapping_token_is_rejected() {
        let trade = Trade {
            inputs: vec![(token(1), BigInt::from(10))],
            outputs: vec![(token(1), BigInt::from(1))],
            wrapped: None,
            gas_ceiling: None,
        };
        assert!(matches!(
            query(&trade, &[]),
            Err(TradeError::Search(SearchError::Overlap))
        ));
    }

    fn pool_byte(action: &Action) -> u8 {
        match action {
            Action::Swap(swap) => swap.contract.0[0],
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_merged_conduit_stays_one_edge() {
        let set = route_set(
            vec![conduit(
                token(1),
                token(2),
                vec![segment(4, 2, 1, 4), segment(4, 1, 1, 5)],
            )],
            vec![vec![0]],
        );
        let built = problem(&sample(), &[], &set, &BTreeMap::new(), name_pool).expect("problem");
        assert_eq!(built.edges.len(), 1);
        assert_eq!(built.routes.len(), 1);
        assert_eq!(built.routes[0].edges, vec![0]);
        let Curve::Chords(chords) = &built.edges[0].curve else {
            panic!("chords");
        };
        assert_eq!(chords.len(), 2);
        assert_eq!(chords[0].width, BigInt::from(4));
        assert_eq!(chords[0].output, BigInt::from(2));
        assert_eq!(pool_byte(&chords[0].action), 4);
        assert_eq!(chords[1].width, BigInt::from(4));
        assert_eq!(chords[1].output, BigInt::from(1));
        assert_eq!(pool_byte(&chords[1].action), 5);
        assert_eq!(built.edges[0].action, chords[0].action);
    }

    #[test]
    fn one_conduit_keeps_chords_in_stored_order() {
        let set = route_set(
            vec![conduit(
                token(1),
                token(2),
                vec![
                    segment(10, 7, 1, 4),
                    segment(5, 1, 1, 5),
                    segment(3, 1, 1, 4),
                ],
            )],
            vec![vec![0]],
        );
        let built = problem(&sample(), &[], &set, &BTreeMap::new(), name_pool).expect("problem");
        assert_eq!(built.edges.len(), 1);
        assert_eq!(built.routes.len(), 1);
        assert_eq!(built.routes[0].edges, vec![0]);
        let Curve::Chords(chords) = &built.edges[0].curve else {
            panic!("chords");
        };
        assert_eq!(
            chords
                .iter()
                .map(|chord| (
                    chord.width.clone(),
                    chord.output.clone(),
                    pool_byte(&chord.action)
                ))
                .collect::<Vec<_>>(),
            vec![
                (BigInt::from(10), BigInt::from(7), 4),
                (BigInt::from(5), BigInt::from(1), 5),
                (BigInt::from(3), BigInt::from(1), 4),
            ]
        );
    }

    #[test]
    fn a_path_keeps_each_conduit() {
        let set = RouteSet {
            inputs: vec![(token(1), BigUint::from(10_u8))],
            outputs: vec![token(3)],
            edges: vec![
                conduit(
                    token(1),
                    token(2),
                    vec![segment(4, 4, 1, 1), segment(4, 3, 1, 2)],
                ),
                conduit(token(2), token(3), vec![segment(4, 4, 1, 3)]),
            ],
            routes: vec![searcher::Route { edges: vec![0, 1] }],
        };
        let trade = Trade {
            inputs: vec![(token(1), BigInt::from(10))],
            outputs: vec![(token(3), BigInt::from(1))],
            wrapped: None,
            gas_ceiling: None,
        };
        let built = problem(&trade, &[], &set, &BTreeMap::new(), name_pool).expect("problem");
        assert_eq!(built.edges.len(), 2);
        assert_eq!(built.routes.len(), 1);
        assert_eq!(built.routes[0].edges, vec![0, 1]);
        let Curve::Chords(first) = &built.edges[0].curve else {
            panic!("chords");
        };
        assert_eq!(pool_byte(&first[0].action), 1);
        assert_eq!(pool_byte(&first[1].action), 2);
    }

    #[test]
    fn a_non_positive_segment_is_dropped() {
        let set = route_set(
            vec![conduit(
                token(1),
                token(2),
                vec![
                    segment(0, 5, 1, 1),
                    segment(4, 2, 1, 2),
                    segment(3, 0, 1, 3),
                ],
            )],
            vec![vec![0]],
        );
        let built = problem(&sample(), &[], &set, &BTreeMap::new(), name_pool).expect("problem");
        assert_eq!(built.edges.len(), 1);
        assert_eq!(built.routes.len(), 1);
    }

    #[test]
    fn an_empty_curve_is_rejected() {
        let empty = route_set(vec![conduit(token(1), token(2), vec![])], vec![vec![0]]);
        assert!(matches!(
            problem(&sample(), &[], &empty, &BTreeMap::new(), name_pool),
            Err(TradeError::Curve)
        ));
        let dead = route_set(
            vec![conduit(token(1), token(2), vec![segment(0, 1, 1, 1)])],
            vec![vec![0]],
        );
        assert!(matches!(
            problem(&sample(), &[], &dead, &BTreeMap::new(), name_pool),
            Err(TradeError::Curve)
        ));
    }

    #[test]
    fn a_wrap_becomes_an_unlimited_custodial_action() {
        let into = RouteSet {
            inputs: vec![(NATIVE, BigUint::from(10_u8))],
            outputs: vec![token(1)],
            edges: vec![searcher::Edge {
                token_in: NATIVE,
                token_out: token(1),
                curve: searcher::Curve::Wrap,
            }],
            routes: vec![searcher::Route { edges: vec![0] }],
        };
        let wrap = Trade {
            inputs: vec![(NATIVE, BigInt::from(10))],
            outputs: vec![(token(1), BigInt::from(1))],
            wrapped: Some(token(1)),
            gas_ceiling: None,
        };
        let built = problem(&wrap, &[], &into, &BTreeMap::new(), name_pool).expect("wrap");
        assert_eq!(built.edges.len(), 1);
        assert!(matches!(built.edges[0].curve, Curve::Unlimited));
        match &built.edges[0].action {
            Action::Wrap(wrap) => {
                assert_eq!(wrap.wrapper, token(1));
                assert_eq!(wrap.underlying, NATIVE);
            }
            other => panic!("{other:?}"),
        }

        let out_of = RouteSet {
            inputs: vec![(token(1), BigUint::from(10_u8))],
            outputs: vec![NATIVE],
            edges: vec![searcher::Edge {
                token_in: token(1),
                token_out: NATIVE,
                curve: searcher::Curve::Wrap,
            }],
            routes: vec![searcher::Route { edges: vec![0] }],
        };
        let unwrap = Trade {
            inputs: vec![(token(1), BigInt::from(10))],
            outputs: vec![(NATIVE, BigInt::from(1))],
            wrapped: Some(token(1)),
            gas_ceiling: None,
        };
        let built = problem(&unwrap, &[], &out_of, &BTreeMap::new(), name_pool).expect("unwrap");
        match &built.edges[0].action {
            Action::Unwrap(unwrap) => {
                assert_eq!(unwrap.wrapper, token(1));
                assert_eq!(unwrap.underlying, NATIVE);
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(built.edges[0].curve, Curve::Unlimited));
    }

    #[test]
    fn a_wrap_off_the_wrapped_token_is_rejected() {
        let set = RouteSet {
            inputs: vec![(NATIVE, BigUint::from(10_u8))],
            outputs: vec![token(2)],
            edges: vec![searcher::Edge {
                token_in: NATIVE,
                token_out: token(2),
                curve: searcher::Curve::Wrap,
            }],
            routes: vec![searcher::Route { edges: vec![0] }],
        };
        let trade = Trade {
            inputs: vec![(NATIVE, BigInt::from(10))],
            outputs: vec![(token(2), BigInt::from(1))],
            wrapped: Some(token(1)),
            gas_ceiling: None,
        };
        assert!(matches!(
            problem(&trade, &[], &set, &BTreeMap::new(), name_pool),
            Err(TradeError::Wrap)
        ));
        let mut bare = trade;
        bare.wrapped = None;
        assert!(matches!(
            problem(&bare, &[], &set, &BTreeMap::new(), name_pool),
            Err(TradeError::Wrap)
        ));
    }

    #[test]
    fn weights_gas_and_the_ceiling_land_on_the_problem() {
        let set = route_set(
            vec![conduit(token(1), token(2), vec![segment(10, 7, 1, 4)])],
            vec![vec![0]],
        );
        let action = name_pool(token(1), token(2), &key(1, 4)).expect("swap");
        let mut gas = BTreeMap::new();
        gas.insert(action.clone(), BigInt::from(9));
        let trade = Trade {
            inputs: vec![(token(1), BigInt::from(10))],
            outputs: vec![(token(2), BigInt::from(3))],
            wrapped: None,
            gas_ceiling: Some(BigInt::from(100)),
        };
        let built = problem(&trade, &[token(9)], &set, &gas, name_pool).expect("problem");
        assert_eq!(built.outputs[0].weight, BigInt::from(3));
        assert_eq!(built.gas.get(&action), Some(&BigInt::from(9)));
        assert_eq!(built.gas_ceiling, Some(BigInt::from(100)));
        assert_eq!(built.hubs, vec![token(9)]);
    }

    #[test]
    fn a_negative_gas_amount_is_rejected() {
        let set = route_set(
            vec![conduit(token(1), token(2), vec![segment(10, 7, 1, 4)])],
            vec![vec![0]],
        );
        let action = name_pool(token(1), token(2), &key(1, 4)).expect("swap");
        let mut gas = BTreeMap::new();
        gas.insert(action, BigInt::from(-1));
        assert!(matches!(
            problem(&sample(), &[], &set, &gas, name_pool),
            Err(TradeError::Gas)
        ));
    }

    #[test]
    fn a_route_set_for_another_trade_is_rejected() {
        let mut set = route_set(
            vec![conduit(token(1), token(2), vec![segment(4, 2, 1, 1)])],
            vec![vec![0]],
        );
        set.outputs = vec![token(9)];
        assert!(matches!(
            problem(&sample(), &[], &set, &BTreeMap::new(), name_pool),
            Err(TradeError::RouteSet)
        ));
        set.outputs = vec![token(2)];
        set.routes = vec![searcher::Route { edges: vec![3] }];
        assert!(matches!(
            problem(&sample(), &[], &set, &BTreeMap::new(), name_pool),
            Err(TradeError::RouteSet)
        ));
    }

    #[test]
    fn allocate_funds_the_chord() {
        let book = Direct(vec![segment(10, 7, 1, 4)]);
        let plan = allocate(&sample(), &[], &book, &BTreeMap::new(), name_pool).expect("plan");
        assert_eq!(plan.count, BigInt::from(7));
        assert_eq!(plan.gas, BigInt::from(0));
    }

    #[test]
    fn allocate_funds_both_pools_of_one_conduit() {
        let book = Direct(vec![segment(1, 4, 1, 1), segment(3, 3, 1, 2)]);
        let mut gas = BTreeMap::new();
        gas.insert(
            name_pool(token(1), token(2), &key(1, 1)).expect("swap"),
            BigInt::from(5),
        );
        gas.insert(
            name_pool(token(1), token(2), &key(1, 2)).expect("swap"),
            BigInt::from(7),
        );
        let plan = allocate(&sample(), &[], &book, &gas, name_pool).expect("plan");
        assert_eq!(plan.count, BigInt::from(7));
        assert_eq!(plan.gas, BigInt::from(12));
        assert_eq!(plan.tenders.len(), 2);
        let mut inputs: Vec<(u8, BigInt)> = plan
            .tenders
            .iter()
            .map(|tender| (pool_byte(&tender.action), tender.input.clone()))
            .collect();
        inputs.sort();
        assert_eq!(inputs, vec![(1, BigInt::from(1)), (2, BigInt::from(3))]);
    }
}
