use std::collections::{BTreeMap, BTreeSet, HashMap};

use num_bigint::{BigInt, BigUint};
use num_traits::Zero;
use routing::{quote, SlopeSegment};
use searcher::{execute, prepare, Conduits, Curve as SearchCurve, Trade, NATIVE};

use crate::{optimize, Chord, Curve, Edge, Input, OptimizeError, Output, Problem, Route, Token};

fn z(value: i64) -> BigInt {
    BigInt::from(value)
}

fn token(byte: u8) -> Token {
    Token([byte; 20])
}

fn pool_key(pool: u8) -> routing::Action {
    routing::Action {
        kind: 1,
        pool: [pool; 32],
    }
}

fn segment(width: i64, output: i64, pool: u8) -> SlopeSegment {
    SlopeSegment {
        width: z(width),
        output: z(output),
        action: pool_key(pool),
    }
}

fn swap_action(token_in: Token, token_out: Token, pool: u8) -> action::Action {
    let contract = action::Address::new([pool; 20]).expect("contract");
    action::Action::Swap(
        action::Swap::new(
            contract,
            contract.pool_id(),
            token_in,
            token_out,
            action::Invariant::Product,
        )
        .expect("swap"),
    )
}

fn chords_of(token_in: u8, token_out: u8, segments: &[SlopeSegment]) -> Vec<Chord> {
    segments
        .iter()
        .map(|segment| Chord {
            width: segment.width.clone(),
            output: segment.output.clone(),
            action: swap_action(token(token_in), token(token_out), segment.action.pool[0]),
        })
        .collect()
}

fn conduit(token_in: u8, token_out: u8, segments: Vec<SlopeSegment>) -> Edge {
    let pool = segments
        .first()
        .map(|segment| segment.action.pool[0])
        .unwrap_or(1);
    Edge {
        action: swap_action(token(token_in), token(token_out), pool),
        curve: Curve::Chords(chords_of(token_in, token_out, &segments)),
    }
}

fn custody_edge(token_in: Token, token_out: Token) -> Edge {
    let action = if token_in == action::NATIVE {
        action::Action::Wrap(action::Wrap::new(token_out, token_in).expect("wrap"))
    } else if token_out == action::NATIVE {
        action::Action::Unwrap(action::Unwrap::new(token_in, token_out).expect("unwrap"))
    } else {
        action::Action::Wrap(action::Wrap::new(token_out, token_in).expect("wrap"))
    };
    Edge {
        action,
        curve: Curve::Unlimited,
    }
}

fn custody(token_in: u8, token_out: u8) -> Edge {
    custody_edge(token(token_in), token(token_out))
}

fn is_pool(action: &action::Action, pool: u8) -> bool {
    match action {
        action::Action::Swap(swap) => swap.contract.0 == [pool; 20],
        _ => false,
    }
}

fn set_gas(problem: &mut Problem, pool: u8, gas: i64) {
    let mut keys = Vec::new();
    for edge in &problem.edges {
        if is_pool(&edge.action, pool) {
            keys.push(edge.action.clone());
        }
        if let Curve::Chords(chords) = &edge.curve {
            for chord in chords {
                if is_pool(&chord.action, pool) {
                    keys.push(chord.action.clone());
                }
            }
        }
    }
    keys.sort();
    keys.dedup();
    for key in keys {
        problem.gas.insert(key, z(gas));
    }
}

fn problem(
    inputs: &[(u8, i64)],
    outputs: &[(u8, i64)],
    edges: Vec<Edge>,
    routes: Vec<Vec<usize>>,
) -> Problem {
    Problem {
        inputs: inputs
            .iter()
            .map(|(byte, amount)| Input {
                token: token(*byte),
                amount: z(*amount),
            })
            .collect(),
        outputs: outputs
            .iter()
            .map(|(byte, weight)| Output {
                token: token(*byte),
                weight: z(*weight),
            })
            .collect(),
        edges,
        routes: routes.into_iter().map(|edges| Route { edges }).collect(),
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    }
}

fn received(plan: &crate::Plan, byte: u8) -> BigInt {
    plan.received
        .iter()
        .find(|(held, _)| *held == token(byte))
        .map(|(_, amount)| amount.clone())
        .unwrap_or_else(BigInt::zero)
}

fn spent(plan: &crate::Plan, byte: u8) -> BigInt {
    plan.spent
        .iter()
        .find(|(held, _)| *held == token(byte))
        .map(|(_, amount)| amount.clone())
        .unwrap_or_else(BigInt::zero)
}

fn funded(problem: &Problem, plan: &crate::Plan) {
    for (input, spent) in problem.inputs.iter().zip(&plan.spent) {
        assert_eq!(input.token, spent.0);
        assert!(spent.1 >= z(0));
        assert!(spent.1 <= input.amount);
    }
    let legs: Vec<(BigInt, BigInt)> = problem
        .outputs
        .iter()
        .zip(&plan.received)
        .map(|(output, (_, held))| (held.clone(), output.weight.clone()))
        .collect();
    let supported = routing::basket::basket_count(&legs).unwrap_or_else(BigInt::zero);
    assert_eq!(plan.count, supported);
    if let Some(ceiling) = &problem.gas_ceiling {
        assert!(&plan.gas <= ceiling);
    }
    let mut gas = z(0);
    let mut seen = BTreeSet::new();
    for tender in &plan.tenders {
        if seen.insert(tender.action.clone()) {
            if let Some(cost) = problem.gas.get(&tender.action) {
                gas += cost;
            }
        }
    }
    assert_eq!(plan.gas, gas);
    settled(problem, plan);
}

#[test]
fn one_budget_stops_at_the_last_count_that_fits() {
    // Width 2 pays 1, so the required input is twice the count.
    let problem = problem(
        &[(1, 40)],
        &[(2, 1)],
        vec![conduit(1, 2, vec![segment(80, 40, 1)])],
        vec![vec![0]],
    );
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(20));
    assert_eq!(spent(&plan, 1), z(40));
    assert_eq!(received(&plan, 2), z(20));
    assert!(plan.tenders[0].input == z(40));
}

#[test]
fn two_budgets_bind_at_the_smaller_count() {
    let problem = problem(
        &[(1, 40), (2, 10)],
        &[(3, 1), (4, 1)],
        vec![
            conduit(1, 3, vec![segment(80, 40, 1)]),
            conduit(2, 4, vec![segment(10, 10, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(spent(&plan, 1), z(20));
    assert_eq!(spent(&plan, 2), z(10));
    assert_eq!(received(&plan, 3), z(10));
    assert_eq!(received(&plan, 4), z(10));
}

#[test]
fn a_flat_tail_keeps_the_greatest_count_that_still_fits() {
    let problem = problem(
        &[(1, 100)],
        &[(2, 1)],
        vec![conduit(1, 2, vec![segment(10, 10, 1)])],
        vec![vec![0]],
    );
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(spent(&plan, 1), z(10));
}

#[test]
fn weights_scale_the_count() {
    let one = problem(
        &[(1, 10)],
        &[(2, 2)],
        vec![conduit(1, 2, vec![segment(10, 10, 1)])],
        vec![vec![0]],
    );
    let plan = optimize(&one).expect("curve");
    funded(&one, &plan);
    assert_eq!(plan.count, z(5));
    assert_eq!(received(&plan, 2), z(10));

    let pair = problem(
        &[(1, 9)],
        &[(2, 2), (3, 1)],
        vec![
            conduit(1, 2, vec![segment(9, 9, 1)]),
            conduit(1, 3, vec![segment(9, 9, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&pair).expect("curves");
    funded(&pair, &plan);
    assert_eq!(plan.count, z(3));
    assert_eq!(received(&plan, 2), z(6));
    assert_eq!(received(&plan, 3), z(3));
    assert_eq!(spent(&plan, 1), z(9));
}

#[test]
fn a_wrap_pays_one_for_one() {
    let problem = problem(&[(1, 25)], &[(2, 1)], vec![custody(1, 2)], vec![vec![0]]);
    let plan = optimize(&problem).expect("wrap");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(25));
    assert_eq!(received(&plan, 2), z(25));
    assert_eq!(plan.tenders.len(), 1);
    assert_eq!(plan.tenders[0].input, z(25));
    assert_eq!(plan.gas, z(0));
}

#[test]
fn the_steeper_opening_fills_before_the_deeper_pool() {
    // Reserves 10/20 against 1000/100. The generous pool pays 10 at input 10
    // and 19 at input 1000. The deep pool pays 1 and 50. The segments fall.
    let generous = vec![segment(10, 10, 1), segment(990, 9, 1)];
    let deep = vec![segment(10, 1, 2), segment(990, 49, 2)];
    let small = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![conduit(1, 2, generous.clone()), conduit(1, 2, deep.clone())],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&small).expect("curves");
    funded(&small, &plan);
    let merged = routing::merge(&[generous.clone(), deep.clone()]);
    assert_eq!(received(&plan, 2), quote(&merged, &z(10)));
    assert!(quote(&generous, &z(10)) > quote(&deep, &z(10)));
    assert_eq!(received(&plan, 2), quote(&generous, &z(10)));
    assert!(plan.tenders.iter().all(|tender| is_pool(&tender.action, 1)));

    let large = problem(
        &[(1, 1000)],
        &[(2, 1)],
        vec![conduit(1, 2, generous.clone()), conduit(1, 2, deep.clone())],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&large).expect("curves");
    funded(&large, &plan);
    assert_eq!(received(&plan, 2), quote(&merged, &z(1000)));
    assert!(received(&plan, 2) > quote(&generous, &z(1000)));
    assert!(received(&plan, 2) > quote(&deep, &z(1000)));
    assert!(plan.tenders.iter().any(|tender| is_pool(&tender.action, 2)));
}

#[test]
fn two_legs_pay_more_than_the_direct_conduit() {
    let problem = problem(
        &[(1, 10)],
        &[(3, 1)],
        vec![
            conduit(1, 3, vec![segment(10, 1, 1)]),
            conduit(1, 2, vec![segment(10, 10, 2)]),
            conduit(2, 3, vec![segment(10, 40, 3)]),
        ],
        vec![vec![0], vec![1, 2]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(40));
    assert_eq!(received(&plan, 3), z(40));
    assert_eq!(plan.flows.len(), 1);
    assert_eq!(plan.flows[0].route, 1);
}

#[test]
fn an_equal_slope_across_two_chords_beats_the_shorter_direct_edge() {
    let problem = problem(
        &[(1, 20_000)],
        &[(3, 1)],
        vec![
            conduit(
                1,
                2,
                vec![segment(10_000, 3_000, 2), segment(10_000, 3_000, 2)],
            ),
            conduit(2, 3, vec![segment(20_000, 20_000, 3)]),
            conduit(1, 3, vec![segment(20_000, 6_000, 1)]),
        ],
        vec![vec![0, 1], vec![2]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(6_000));
    assert_eq!(spent(&plan, 1), z(20_000));
    assert_eq!(received(&plan, 3), z(6_000));
    let wide = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 2))
        .expect("equal slope");
    assert_eq!(wide.input, z(20_000));
    assert!(plan
        .tenders
        .iter()
        .all(|tender| !is_pool(&tender.action, 1)));
}

#[test]
fn a_wide_multi_edge_piece_is_taken_whole() {
    let problem = problem(
        &[(1, 2_000_000)],
        &[(3, 1)],
        vec![
            conduit(
                1,
                2,
                vec![
                    segment(1_000_000, 1_500_000, 1),
                    segment(1_000_000, 100_000, 1),
                ],
            ),
            conduit(2, 3, vec![segment(2_000_000, 2_000_000, 2)]),
        ],
        vec![vec![0, 1]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(1_600_000));
    assert_eq!(spent(&plan, 1), z(2_000_000));
    assert_eq!(received(&plan, 3), z(1_600_000));
}

#[test]
fn an_overshoot_leaves_the_shallower_landing_segment() {
    // One unit pays 100 and lands in the shallow segment. Extending through
    // that segment averages far below the direct edge, so the direct edge
    // keeps the rest of the budget.
    let problem = problem(
        &[(1, 20_000)],
        &[(3, 1)],
        vec![
            conduit(1, 3, vec![segment(20_000, 20_000, 1)]),
            conduit(1, 2, vec![segment(20_000, 200_000, 2)]),
            conduit(2, 3, vec![segment(1, 100, 3), segment(100_000, 5_000, 4)]),
        ],
        vec![vec![0], vec![1, 2]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(20_099));
    assert_eq!(spent(&plan, 1), z(20_000));
    assert_eq!(received(&plan, 3), z(20_099));
    let steep = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 3))
        .expect("steep unit");
    assert_eq!(steep.input, z(1));
}

#[test]
fn a_landing_above_the_direct_edge_is_taken_with_it() {
    // One unit overshoots a steep chord and lands in a segment that still
    // pays more than the direct edge. That landing is spent, and the direct
    // edge keeps the source the landing does not use.
    let problem = problem(
        &[(1, 30_000)],
        &[(3, 1)],
        vec![
            conduit(1, 3, vec![segment(30_000, 30_000, 1)]),
            conduit(1, 2, vec![segment(30_000, 300_000, 2)]),
            conduit(2, 3, vec![segment(2, 40, 3), segment(100_000, 300_000, 3)]),
        ],
        vec![vec![0], vec![1, 2]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(320_039));
    assert_eq!(spent(&plan, 1), z(30_000));
    assert_eq!(received(&plan, 3), z(320_039));
    let landing = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 3))
        .expect("landing");
    let direct = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 1))
        .expect("direct edge");
    assert_eq!(landing.input, z(100_002));
    assert_eq!(direct.input, z(19_999));
}

#[test]
fn the_direct_piece_beats_an_opening_that_truncates() {
    // The path's opening pays 40 for one source token. The direct chord pays
    // 10_000 for 100, so the direct chord is filled and the path takes what
    // remains.
    let problem = problem(
        &[(1, 20_000)],
        &[(3, 1)],
        vec![
            conduit(1, 3, vec![segment(100, 10_000, 1)]),
            conduit(1, 2, vec![segment(20_000, 200_000, 2)]),
            conduit(
                2,
                3,
                vec![segment(4, 40, 3), segment(200_000, 2_000_000, 4)],
            ),
        ],
        vec![vec![0], vec![1, 2]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(2_000_000));
    assert_eq!(spent(&plan, 1), z(20_000));
    assert_eq!(received(&plan, 3), z(2_000_000));
    let direct = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 1))
        .expect("direct");
    let hop = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 2))
        .expect("first hop");
    let opening = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 3))
        .expect("opening");
    let landing = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 4))
        .expect("landing");
    assert_eq!(direct.input, z(100));
    assert_eq!(hop.input, z(19_900));
    assert_eq!(opening.input, z(4));
    assert_eq!(landing.input, z(198_996));
}

#[test]
fn a_one_unit_piece_is_spent_before_the_shallower_pool() {
    // The unit pays 1_000_000. The other pool pays 50 across 100.
    // Input 98 and input 99 both pay 49, so the fill spends 98 of that pool.
    let problem = problem(
        &[(1, 100)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(1, 1_000_000, 1)]),
            conduit(1, 2, vec![segment(100, 50, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(1_000_049));
    assert_eq!(spent(&plan, 1), z(99));
    assert_eq!(received(&plan, 2), z(1_000_049));
    let steep = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 1))
        .expect("one-unit piece");
    let shallow = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 2))
        .expect("shallower pool");
    assert_eq!(steep.input, z(1));
    assert_eq!(shallow.input, z(98));
}

#[test]
fn shared_paths_spend_the_budget_once() {
    let mut edges = Vec::new();
    let mut routes = Vec::new();
    for pool in 1..=8 {
        edges.push(conduit(1, 2, vec![segment(10, 10, pool)]));
        routes.push(vec![edges.len() - 1]);
    }
    let problem = problem(&[(1, 10)], &[(2, 1)], edges, routes);
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(spent(&plan, 1), z(10));
    assert_eq!(received(&plan, 2), z(10));
    assert_eq!(plan.tenders.len(), 1);
}

#[test]
fn the_wider_equal_slope_takes_the_budget() {
    let mut edges = vec![conduit(1, 2, vec![segment(11, 11, 12)])];
    let mut routes = vec![vec![0]];
    for pool in 1..=11_u8 {
        edges.push(conduit(1, 2, vec![segment(1, 1, pool)]));
        routes.push(vec![edges.len() - 1]);
    }
    let mut problem = problem(&[(1, 11)], &[(2, 1)], edges, routes);
    set_gas(&mut problem, 12, 50);
    for pool in 1..=11_u8 {
        set_gas(&mut problem, pool, 1);
    }
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(11));
    assert_eq!(plan.gas, z(50));
    assert_eq!(plan.tenders.len(), 1);
    assert!(is_pool(&plan.tenders[0].action, 12));
}

#[test]
fn leftover_intermediate_balance_funds_the_other_path() {
    // Input 1 pays 2 into the intermediate, and the next edge takes 1.
    // Nothing of the source budget remains. The other path spends the 1
    // that is left, and its own producer stays closed.
    let problem = problem(
        &[(1, 1)],
        &[(4, 1)],
        vec![
            conduit(1, 2, vec![segment(2, 4, 1)]),
            conduit(2, 4, vec![segment(1, 1, 2)]),
            conduit(1, 2, vec![segment(1, 1, 3)]),
            conduit(2, 4, vec![segment(10, 10, 4)]),
        ],
        vec![vec![0, 1], vec![2, 3]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(2));
    assert_eq!(spent(&plan, 1), z(1));
    assert_eq!(received(&plan, 4), z(2));
    let funded_by_leftover = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 4))
        .expect("path that spends the intermediate");
    assert_eq!(funded_by_leftover.input, z(1));
    assert!(plan
        .tenders
        .iter()
        .all(|tender| !is_pool(&tender.action, 3)));
    let other = plan
        .flows
        .iter()
        .find(|flow| flow.route == 1)
        .expect("other path");
    assert_eq!(other.input, z(0));
    assert_eq!(other.output, z(1));
}

#[test]
fn a_count_above_the_curve_is_not_a_plan() {
    let problem = problem(
        &[(1, 100)],
        &[(2, 1)],
        vec![conduit(1, 2, vec![segment(10, 10, 1)])],
        vec![vec![0]],
    );
    assert!(crate::solve::meet(&problem, &z(40)).is_none());
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(spent(&plan, 1), z(10));
    assert_eq!(received(&plan, 2), z(10));
}

#[test]
fn identical_curves_split_a_shared_budget() {
    let curve = cp_curve(60, 60, &[10, 20, 30, 40, 60], 1);
    let other = cp_curve(60, 60, &[10, 20, 30, 40, 60], 2);
    let problem = problem(
        &[(1, 60)],
        &[(2, 1), (3, 1)],
        vec![conduit(1, 2, curve), conduit(1, 3, other)],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(20));
    assert!(received(&plan, 2) >= z(20));
    assert!(received(&plan, 3) >= z(20));
}

#[test]
fn a_shared_conduit_is_one_tender_and_both_outputs_are_funded() {
    let shared = vec![segment(4, 8, 1), segment(6, 2, 1)];
    let problem = problem(
        &[(1, 10)],
        &[(3, 1), (4, 1)],
        vec![
            conduit(1, 2, shared.clone()),
            conduit(2, 3, vec![segment(20, 20, 2)]),
            conduit(2, 4, vec![segment(20, 20, 3)]),
        ],
        vec![vec![0, 1], vec![0, 2]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(5));
    assert!(received(&plan, 3) >= z(5));
    assert!(received(&plan, 4) >= z(5));
    assert_eq!(plan.tenders.len(), 3);
    let shared_tender = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 1))
        .expect("shared action");
    assert_eq!(shared_tender.input, z(10));
    assert_eq!(quote(&shared, &shared_tender.input), z(10));

    let mut reversed = problem.clone();
    reversed.routes.reverse();
    let again = optimize(&reversed).expect("curves");
    funded(&reversed, &again);
    assert_eq!(again.count, plan.count);
    assert_eq!(again.tenders, plan.tenders);
}

#[test]
fn a_route_that_spends_an_output_is_rejected() {
    let problem = problem(
        &[(1, 10)],
        &[(2, 1), (3, 1)],
        vec![
            conduit(1, 2, vec![segment(10, 10, 1)]),
            conduit(2, 3, vec![segment(10, 10, 2)]),
        ],
        vec![vec![0], vec![0, 1]],
    );
    assert_eq!(optimize(&problem), Err(OptimizeError::Cycle));
}

#[test]
fn equal_slopes_fill_the_earlier_curve() {
    let mut problem = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(10, 10, 1)]),
            conduit(1, 2, vec![segment(10, 10, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    set_gas(&mut problem, 1, 5);
    set_gas(&mut problem, 2, 1);
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(plan.gas, z(5));
    assert_eq!(plan.tenders.len(), 1);
    assert!(is_pool(&plan.tenders[0].action, 1));
}

#[test]
fn a_larger_basket_keeps_the_action_with_more_gas() {
    let mut problem = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(10, 10, 1)]),
            conduit(1, 2, vec![segment(10, 12, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    set_gas(&mut problem, 1, 1);
    set_gas(&mut problem, 2, 100);
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(12));
    assert_eq!(plan.gas, z(100));
    assert!(!routing::basket::open_count_wins(&z(12), &z(10), &z(100)));
}

#[test]
fn a_loose_ceiling_funds_the_same_wide_route() {
    let mut problem = problem(
        &[(1, 100_000)],
        &[(3, 1)],
        vec![
            conduit(
                1,
                2,
                vec![segment(40_000, 40_000, 1), segment(60_000, 60_000, 2)],
            ),
            conduit(2, 3, vec![segment(100_000, 100_000, 3)]),
        ],
        vec![vec![0, 1]],
    );
    set_gas(&mut problem, 1, 10);
    set_gas(&mut problem, 2, 50);
    set_gas(&mut problem, 3, 1);
    let open = optimize(&problem).expect("curves");
    funded(&problem, &open);
    problem.gas_ceiling = Some(open.gas.clone());
    let capped = optimize(&problem).expect("curves");
    funded(&problem, &capped);
    assert_eq!(capped.count, open.count);
    assert_eq!(capped.gas, open.gas);
    assert_eq!(capped.flows, open.flows);
}

#[test]
fn a_ceiling_stops_before_the_next_action_on_a_wide_route() {
    let mut problem = problem(
        &[(1, 100_000)],
        &[(3, 1)],
        vec![
            conduit(
                1,
                2,
                vec![segment(40_000, 40_000, 1), segment(60_000, 60_000, 2)],
            ),
            conduit(2, 3, vec![segment(100_000, 100_000, 3)]),
        ],
        vec![vec![0, 1]],
    );
    set_gas(&mut problem, 1, 10);
    set_gas(&mut problem, 2, 50);
    set_gas(&mut problem, 3, 1);
    problem.gas_ceiling = Some(z(11));
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(40_000));
    assert_eq!(plan.gas, z(11));
    assert_eq!(spent(&plan, 1), z(40_000));
}

#[test]
fn a_gas_ceiling_leaves_the_larger_action_closed() {
    let mut problem = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(10, 10, 1)]),
            conduit(1, 2, vec![segment(10, 12, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    set_gas(&mut problem, 1, 1);
    set_gas(&mut problem, 2, 100);
    problem.gas_ceiling = Some(z(1));
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(plan.gas, z(1));
    assert!(is_pool(&plan.tenders[0].action, 1));
}

#[test]
fn wrap_gas_is_charged_once_on_the_wrap_that_carries_flow() {
    let wrap = custody(1, 2);
    let mut problem = problem(
        &[(1, 4)],
        &[(3, 1)],
        vec![wrap.clone(), conduit(2, 3, vec![segment(4, 4, 1)])],
        vec![vec![0, 1]],
    );
    problem.gas.insert(wrap.action, z(7));
    set_gas(&mut problem, 1, 3);
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(4));
    assert_eq!(plan.gas, z(10));
}

#[test]
fn a_hub_breaks_a_tie_toward_the_path_that_touches_it() {
    let mut problem = problem(
        &[(1, 10)],
        &[(3, 1)],
        vec![
            conduit(1, 3, vec![segment(10, 10, 1)]),
            conduit(1, 9, vec![segment(10, 10, 2)]),
            conduit(9, 3, vec![segment(10, 10, 3)]),
        ],
        vec![vec![0], vec![1, 2]],
    );
    problem.hubs = vec![token(9)];
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(plan.flows.len(), 1);
    assert_eq!(plan.flows[0].route, 1);
}

#[test]
fn parallel_routes_match_an_exhaustive_split() {
    let curves = [
        vec![segment(4, 6, 1), segment(4, 2, 1)],
        vec![segment(3, 3, 2), segment(5, 2, 2)],
        vec![segment(6, 5, 3)],
    ];
    for budget in 1..9 {
        for left in 0..curves.len() {
            for right in 0..curves.len() {
                let problem = problem(
                    &[(1, budget)],
                    &[(2, 1)],
                    vec![
                        conduit(1, 2, curves[left].clone()),
                        conduit(1, 2, curves[right].clone()),
                    ],
                    vec![vec![0], vec![1]],
                );
                let plan = optimize(&problem).expect("curves");
                funded(&problem, &plan);
                let mut best = z(0);
                for spend in 0..=budget {
                    let out =
                        quote(&curves[left], &z(spend)) + quote(&curves[right], &z(budget - spend));
                    if out > best {
                        best = out;
                    }
                }
                assert_eq!(received(&plan, 2), best);
            }
        }
    }
}

#[test]
fn two_outputs_match_an_exhaustive_split() {
    let left = vec![segment(2, 2, 1), segment(4, 2, 1)];
    let right = vec![segment(3, 3, 2), segment(3, 1, 2)];
    for budget in 1..9 {
        let problem = problem(
            &[(1, budget)],
            &[(2, 1), (3, 1)],
            vec![conduit(1, 2, left.clone()), conduit(1, 3, right.clone())],
            vec![vec![0], vec![1]],
        );
        let plan = optimize(&problem).expect("curves");
        funded(&problem, &plan);
        let mut best = z(0);
        for spend in 0..=budget {
            let count = std::cmp::min(quote(&left, &z(spend)), quote(&right, &z(budget - spend)));
            if count > best {
                best = count;
            }
        }
        assert_eq!(plan.count, best);
    }
}

#[test]
fn a_path_matches_an_exhaustive_split_against_the_direct_conduit() {
    let direct = vec![segment(3, 2, 1), segment(5, 2, 1)];
    let first = vec![segment(4, 5, 2), segment(4, 2, 2)];
    let second = vec![segment(4, 4, 3), segment(4, 2, 3)];
    for budget in 1..7 {
        let problem = problem(
            &[(1, budget)],
            &[(3, 1)],
            vec![
                conduit(1, 3, direct.clone()),
                conduit(1, 2, first.clone()),
                conduit(2, 3, second.clone()),
            ],
            vec![vec![0], vec![1, 2]],
        );
        let plan = optimize(&problem).expect("curves");
        funded(&problem, &plan);
        let mut best = z(0);
        for spend in 0..=budget {
            let mid = quote(&first, &z(budget - spend));
            let far = quote(&second, &mid);
            let out = quote(&direct, &z(spend)) + far;
            if out > best {
                best = out;
            }
        }
        assert_eq!(received(&plan, 3), best);
    }
}

#[test]
fn a_shared_conduit_matches_splitting_its_quote() {
    let shared = vec![segment(3, 5, 1), segment(4, 3, 1)];
    for budget in 1..8 {
        let problem = problem(
            &[(1, budget)],
            &[(3, 1), (4, 1)],
            vec![
                conduit(1, 2, shared.clone()),
                conduit(2, 3, vec![segment(20, 20, 2)]),
                conduit(2, 4, vec![segment(20, 20, 3)]),
            ],
            vec![vec![0, 1], vec![0, 2]],
        );
        let plan = optimize(&problem).expect("curves");
        funded(&problem, &plan);
        let total = quote(&shared, &z(budget));
        assert_eq!(plan.count, &total / 2);
    }
}

#[test]
fn raising_the_amount_does_not_lower_the_count() {
    let curve = vec![segment(3, 4, 1), segment(5, 3, 1), segment(4, 1, 1)];
    let mut previous = z(0);
    for budget in 1..15 {
        let problem = problem(
            &[(1, budget)],
            &[(2, 1)],
            vec![conduit(1, 2, curve.clone())],
            vec![vec![0]],
        );
        let plan = optimize(&problem).expect("curve");
        funded(&problem, &plan);
        assert!(plan.count >= previous);
        previous = plan.count;
    }
}

#[test]
fn dropping_the_steeper_route_lowers_the_small_basket() {
    let generous = vec![segment(10, 10, 1), segment(990, 9, 1)];
    let deep = vec![segment(10, 1, 2), segment(990, 49, 2)];
    let both = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![conduit(1, 2, generous), conduit(1, 2, deep.clone())],
        vec![vec![0], vec![1]],
    );
    let only_deep = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![conduit(1, 2, deep)],
        vec![vec![0]],
    );
    let full = optimize(&both).expect("curves");
    let dropped = optimize(&only_deep).expect("curve");
    assert!(full.count > dropped.count);
}

#[test]
fn a_rising_slope_is_rejected() {
    let problem = problem(
        &[(1, 5)],
        &[(2, 1)],
        vec![conduit(1, 2, vec![segment(2, 2, 1), segment(2, 4, 1)])],
        vec![vec![0]],
    );
    assert_eq!(optimize(&problem), Err(OptimizeError::BrokenCurve));
}

#[test]
fn a_token_on_both_sides_is_rejected() {
    let problem = problem(
        &[(1, 5)],
        &[(1, 1)],
        vec![conduit(1, 2, vec![segment(2, 2, 1)])],
        vec![vec![0]],
    );
    assert_eq!(optimize(&problem), Err(OptimizeError::Overlap));
}

#[test]
fn a_route_that_repeats_a_token_is_rejected() {
    let problem = problem(
        &[(1, 5)],
        &[(2, 1)],
        vec![
            conduit(1, 3, vec![segment(2, 2, 1)]),
            conduit(3, 1, vec![segment(2, 2, 2)]),
            conduit(1, 2, vec![segment(2, 2, 3)]),
        ],
        vec![vec![0, 1, 2]],
    );
    assert_eq!(optimize(&problem), Err(OptimizeError::Cycle));
}

#[test]
fn the_searcher_route_set_funds_the_hub_path() {
    let hub = search_token(9);
    let query = prepare(search_trade(&[(1, 10)], &[3]), &[hub], None).expect("trade");
    let catalog = Catalog(HashMap::from([
        ((search_token(1), search_token(3)), vec![segment(10, 1, 1)]),
        ((search_token(1), hub), vec![segment(10, 10, 2)]),
        ((hub, search_token(3)), vec![segment(10, 40, 3)]),
    ]));
    let set = execute(&query, &catalog);
    let mut problem = from_route_set(&set);
    problem.hubs = vec![token(9)];
    let plan = optimize(&problem).expect("route set");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(40));
    assert!(plan.flows.iter().any(|flow| flow.output == z(40)));
    assert!(plan
        .flows
        .iter()
        .all(|flow| problem.routes[flow.route].edges.len() == 2));
}

#[test]
fn the_searcher_route_set_shares_the_hub_conduit() {
    let hub = search_token(9);
    let query = prepare(search_trade(&[(1, 10)], &[2, 3]), &[hub], None).expect("trade");
    let catalog = Catalog(HashMap::from([
        ((search_token(1), hub), vec![segment(10, 10, 1)]),
        ((hub, search_token(2)), vec![segment(10, 10, 2)]),
        ((hub, search_token(3)), vec![segment(10, 10, 3)]),
    ]));
    let set = execute(&query, &catalog);
    assert_eq!(set.routes.len(), 2);
    assert_eq!(set.routes[0].edges[0], set.routes[1].edges[0]);
    let problem = from_route_set(&set);
    let plan = optimize(&problem).expect("route set");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(5));
    assert!(received(&plan, 2) >= z(5));
    assert!(received(&plan, 3) >= z(5));
    assert_eq!(
        plan.tenders
            .iter()
            .find(|tender| is_pool(&tender.action, 1))
            .expect("hub conduit")
            .input,
        z(10)
    );
}

#[test]
fn the_searcher_wrap_is_one_for_one_into_the_conduit() {
    let wrapped = search_token(1);
    let out = search_token(2);
    let query = prepare(
        Trade {
            inputs: vec![(NATIVE, BigUint::from(10_u8))],
            outputs: vec![out],
        },
        &[],
        Some(wrapped),
    )
    .expect("trade");
    let catalog = Catalog(HashMap::from([((wrapped, out), vec![segment(10, 7, 4)])]));
    let set = execute(&query, &catalog);
    assert!(matches!(set.edges[0].curve, SearchCurve::Wrap));
    let mut problem = from_route_set(&set);
    let wrap = problem
        .edges
        .iter()
        .find(|edge| matches!(edge.curve, Curve::Unlimited))
        .expect("wrap")
        .action
        .clone();
    problem.gas.insert(wrap, z(2));
    set_gas(&mut problem, 4, 5);
    let plan = optimize(&problem).expect("route set");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(7));
    assert_eq!(plan.gas, z(7));
}

fn actions_of(edge: &Edge) -> Vec<action::Action> {
    match &edge.curve {
        Curve::Unlimited => vec![edge.action.clone()],
        Curve::Chords(chords) => {
            let mut actions = Vec::new();
            for chord in chords {
                if !actions.contains(&chord.action) {
                    actions.push(chord.action.clone());
                }
            }
            actions
        }
    }
}

fn settled(problem: &Problem, plan: &crate::Plan) {
    let mut owners = BTreeMap::new();
    for (index, edge) in problem.edges.iter().enumerate() {
        for action in actions_of(edge) {
            if let Some(previous) = owners.insert(action, index) {
                if previous != index {
                    return;
                }
            }
        }
    }
    let mut incoming: HashMap<Token, BigInt> = HashMap::new();
    let mut outgoing: HashMap<Token, BigInt> = HashMap::new();
    for edge in &problem.edges {
        let Some((token_in, token_out)) = edge.token_in().zip(edge.token_out()) else {
            return;
        };
        let actions = actions_of(edge);
        let input = plan
            .tenders
            .iter()
            .filter(|tender| actions.iter().any(|action| action == &tender.action))
            .fold(BigInt::zero(), |sum, tender| sum + &tender.input);
        *outgoing.entry(token_in).or_default() += &input;
        *incoming.entry(token_out).or_default() += crate::quote_curve(&edge.curve, &input);
    }
    for input in &problem.inputs {
        let spent = outgoing
            .get(&input.token)
            .cloned()
            .unwrap_or_else(BigInt::zero);
        assert!(spent <= input.amount);
        assert_eq!(spent, spent_of(plan, input.token));
        assert!(incoming
            .get(&input.token)
            .map(Zero::is_zero)
            .unwrap_or(true));
    }
    for output in &problem.outputs {
        let produced = incoming
            .get(&output.token)
            .cloned()
            .unwrap_or_else(BigInt::zero);
        let consumed = outgoing
            .get(&output.token)
            .cloned()
            .unwrap_or_else(BigInt::zero);
        assert!(produced >= consumed);
        assert_eq!(&produced - &consumed, received(plan, output.token.0[0]));
    }
}

fn spent_of(plan: &crate::Plan, token: Token) -> BigInt {
    plan.spent
        .iter()
        .find(|(held, _)| *held == token)
        .map(|(_, amount)| amount.clone())
        .unwrap_or_else(BigInt::zero)
}

#[test]
fn the_steeper_curve_funds_its_leg_of_the_basket() {
    let mut problem = problem(
        &[(1, 10)],
        &[(2, 1), (3, 1)],
        vec![
            conduit(1, 2, vec![segment(5, 6, 1)]),
            conduit(1, 2, vec![segment(10, 10, 2)]),
            conduit(1, 3, vec![segment(10, 10, 3)]),
        ],
        vec![vec![0], vec![1], vec![2]],
    );
    set_gas(&mut problem, 1, 100);
    set_gas(&mut problem, 2, 1);
    set_gas(&mut problem, 3, 1);
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(5));
    assert_eq!(plan.gas, z(101));
    assert!(plan.tenders.iter().any(|tender| is_pool(&tender.action, 1)));
}

#[test]
fn equal_slopes_spend_the_earlier_chords() {
    let mut problem = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(5, 5, 1)]),
            conduit(1, 2, vec![segment(5, 5, 2)]),
            conduit(1, 2, vec![segment(10, 10, 3)]),
        ],
        vec![vec![0], vec![1], vec![2]],
    );
    set_gas(&mut problem, 1, 3);
    set_gas(&mut problem, 2, 3);
    set_gas(&mut problem, 3, 4);
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(plan.gas, z(6));
    assert_eq!(plan.tenders.len(), 2);
    assert!(plan.tenders.iter().any(|tender| is_pool(&tender.action, 1)));
    assert!(plan.tenders.iter().any(|tender| is_pool(&tender.action, 2)));
}

#[test]
fn equal_widths_fill_the_earlier_curve() {
    let mut problem = problem(
        &[(1, 4)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(4, 4, 1)]),
            conduit(1, 2, vec![segment(4, 4, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    set_gas(&mut problem, 1, 10);
    set_gas(&mut problem, 2, 1);
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(4));
    assert_eq!(plan.gas, z(10));
    assert_eq!(plan.tenders.len(), 1);
    assert!(is_pool(&plan.tenders[0].action, 1));
}

#[test]
fn a_conduit_attributes_input_along_the_merged_slope() {
    let problem = problem(
        &[(1, 4)],
        &[(2, 1)],
        vec![conduit(1, 2, vec![segment(1, 4, 1), segment(3, 3, 2)])],
        vec![vec![0]],
    );
    let plan = optimize(&problem).expect("conduit");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(7));
    assert_eq!(spent(&plan, 1), z(4));
    assert_eq!(received(&plan, 2), z(7));
    assert_eq!(plan.tenders.len(), 2);
    let mut inputs: Vec<(u8, BigInt)> = plan
        .tenders
        .iter()
        .map(|tender| {
            let action::Action::Swap(swap) = &tender.action else {
                panic!("{:?}", tender.action);
            };
            (swap.contract.0[0], tender.input.clone())
        })
        .collect();
    inputs.sort();
    assert_eq!(inputs, vec![(1, z(1)), (2, z(3))]);
}

#[test]
fn the_opening_chord_takes_a_budget_that_fits_it() {
    let mut problem = problem(
        &[(1, 2)],
        &[(2, 1)],
        vec![conduit(1, 2, vec![segment(2, 2, 1), segment(2, 2, 2)])],
        vec![vec![0]],
    );
    set_gas(&mut problem, 1, 50);
    set_gas(&mut problem, 2, 1);
    let plan = optimize(&problem).expect("conduit");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(2));
    assert_eq!(plan.gas, z(50));
    assert_eq!(plan.tenders.len(), 1);
    assert!(is_pool(&plan.tenders[0].action, 1));
    assert_eq!(plan.tenders[0].input, z(2));
}

#[test]
fn a_steep_shared_segment_funds_both_outputs() {
    let problem = problem(
        &[(1, 10)],
        &[(3, 1), (4, 1)],
        vec![
            conduit(1, 2, vec![segment(2, 10, 1), segment(8, 2, 1)]),
            conduit(2, 3, vec![segment(20, 20, 2)]),
            conduit(2, 4, vec![segment(20, 20, 3)]),
        ],
        vec![vec![0, 1], vec![0, 2]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(6));
    assert_eq!(spent(&plan, 1), z(10));
}

#[test]
fn a_join_books_one_tender_on_the_shared_curve() {
    let problem = problem(
        &[(1, 6)],
        &[(4, 1)],
        vec![
            conduit(1, 2, vec![segment(10, 10, 2)]),
            conduit(2, 4, vec![segment(4, 6, 1), segment(4, 2, 1)]),
            conduit(1, 3, vec![segment(10, 10, 3)]),
            conduit(3, 2, vec![segment(10, 10, 4)]),
        ],
        vec![vec![0, 1], vec![2, 3, 1]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(7));
    let shared = plan
        .tenders
        .iter()
        .find(|tender| is_pool(&tender.action, 1))
        .expect("shared curve");
    assert_eq!(shared.input, z(6));
    assert_eq!(shared.token_in, token(2));
    assert_eq!(shared.token_out, token(4));
}

#[test]
fn the_same_action_on_two_pairs_keeps_both_quotes() {
    let mut problem = problem(
        &[(1, 10)],
        &[(3, 1)],
        vec![
            conduit(1, 2, vec![segment(10, 10, 1)]),
            conduit(2, 3, vec![segment(10, 10, 1)]),
        ],
        vec![vec![0, 1]],
    );
    set_gas(&mut problem, 1, 5);
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(plan.gas, z(10));
    assert_eq!(plan.tenders.len(), 2);
    assert!(plan.tenders.iter().all(|tender| tender.input == z(10)));
}

#[test]
fn one_action_on_two_edges_opens_once() {
    let mut problem = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(6, 6, 1)]),
            conduit(1, 2, vec![segment(6, 6, 1)]),
        ],
        vec![vec![0], vec![1]],
    );
    set_gas(&mut problem, 1, 5);
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(plan.gas, z(5));
    assert_eq!(plan.tenders.len(), 1);
    assert_eq!(plan.tenders[0].input, z(10));
}

#[test]
fn two_sources_add_their_output() {
    let problem = problem(
        &[(1, 10), (2, 7)],
        &[(3, 1)],
        vec![
            conduit(1, 3, vec![segment(10, 10, 1)]),
            conduit(2, 3, vec![segment(7, 7, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(17));
    assert_eq!(received(&plan, 3), z(17));
}

#[test]
fn a_richer_curve_beats_the_path_that_touches_a_hub() {
    let mut problem = problem(
        &[(1, 10)],
        &[(3, 1)],
        vec![
            conduit(1, 3, vec![segment(10, 10, 1)]),
            conduit(1, 9, vec![segment(10, 9, 2)]),
            conduit(9, 3, vec![segment(9, 9, 3)]),
        ],
        vec![vec![0], vec![1, 2]],
    );
    problem.hubs = vec![token(9)];
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(plan.flows.len(), 1);
    assert_eq!(plan.flows[0].route, 0);
}

#[test]
fn a_wide_budget_funds_the_last_count() {
    let amount = BigInt::from(10).pow(40);
    let mut wide = problem(
        &[(1, 1)],
        &[(2, 1)],
        vec![conduit(1, 2, vec![segment(1, 1, 1)])],
        vec![vec![0]],
    );
    wide.inputs[0].amount = amount.clone();
    wide.edges[0].curve = Curve::Chords(vec![Chord {
        width: amount.clone(),
        output: amount.clone(),
        action: wide.edges[0].action.clone(),
    }]);
    let plan = optimize(&wide).expect("curve");
    funded(&wide, &plan);
    assert_eq!(plan.count, amount);
}

#[test]
fn a_wide_chord_spends_the_least_input() {
    // Width 30000 pays 2, so input 15000 is the first amount that pays 1.
    let mut wide = problem(
        &[(1, 1)],
        &[(2, 1)],
        vec![conduit(1, 2, vec![segment(30_000, 2, 1)])],
        vec![vec![0]],
    );
    wide.inputs[0].amount = z(25_000);
    let plan = optimize(&wide).expect("curve");
    funded(&wide, &plan);
    assert_eq!(plan.count, z(1));
    assert_eq!(spent(&plan, 1), z(15_000));
}

#[test]
fn a_wide_chord_leaves_room_for_the_steep_curve() {
    let problem = problem(
        &[(1, 20_000)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(30_000, 2, 1)]),
            conduit(1, 2, vec![segment(5_000, 1, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(2));
    assert_eq!(spent(&plan, 1), z(20_000));
}

#[test]
fn a_zero_ceiling_keeps_the_free_wrap() {
    let mut problem = problem(
        &[(1, 5)],
        &[(2, 1)],
        vec![custody(1, 2), conduit(1, 2, vec![segment(5, 8, 1)])],
        vec![vec![0], vec![1]],
    );
    set_gas(&mut problem, 1, 1);
    problem.gas_ceiling = Some(z(0));
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(5));
    assert_eq!(plan.gas, z(0));
    assert_eq!(plan.tenders.len(), 1);
    assert!(matches!(plan.tenders[0].action, action::Action::Wrap(_)));
    assert_eq!(plan.tenders[0].input, z(5));
}

#[test]
fn rejected_problems_name_the_fault() {
    let curve = vec![conduit(1, 2, vec![segment(2, 2, 1)])];
    let routes = vec![vec![0]];
    assert_eq!(
        optimize(&problem(
            &[(1, 0)],
            &[(2, 1)],
            curve.clone(),
            routes.clone()
        )),
        Err(OptimizeError::ZeroAmount)
    );
    assert_eq!(
        optimize(&problem(
            &[(1, 5), (1, 5)],
            &[(2, 1)],
            curve.clone(),
            routes.clone()
        )),
        Err(OptimizeError::DuplicateInput)
    );
    assert_eq!(
        optimize(&problem(
            &[(1, 5)],
            &[(3, 1)],
            curve.clone(),
            routes.clone()
        )),
        Err(OptimizeError::Disconnected)
    );
    assert_eq!(
        optimize(&problem(&[(1, 5)], &[(2, 1)], curve.clone(), vec![vec![3]])),
        Err(OptimizeError::RouteEdge)
    );
    assert_eq!(
        optimize(&problem(
            &[(1, 5), (2, 5)],
            &[(4, 1)],
            vec![conduit(1, 2, vec![segment(2, 2, 1)])],
            routes.clone()
        )),
        Err(OptimizeError::SecondInput)
    );
    let mut negative = problem(&[(1, 5)], &[(2, 1)], curve.clone(), routes);
    set_gas(&mut negative, 1, -1);
    assert_eq!(optimize(&negative), Err(OptimizeError::NegativeGas));
    assert_eq!(
        optimize(&problem(
            &[(1, 5)],
            &[(2, 1)],
            vec![conduit(1, 2, vec![])],
            vec![vec![0]]
        )),
        Err(OptimizeError::BrokenCurve)
    );
    let mut unlimited = problem(&[(1, 5)], &[(2, 1)], curve, vec![vec![0]]);
    unlimited.edges[0].curve = Curve::Unlimited;
    assert_eq!(optimize(&unlimited), Err(OptimizeError::BrokenCurve));
    let mut chorded = problem(&[(1, 5)], &[(2, 1)], vec![custody(1, 2)], vec![vec![0]]);
    chorded.edges[0].curve = Curve::Chords(vec![Chord {
        width: z(2),
        output: z(2),
        action: chorded.edges[0].action.clone(),
    }]);
    assert_eq!(optimize(&chorded), Err(OptimizeError::BrokenCurve));
    let contract = action::Address::new([8; 20]).expect("contract");
    let wide = action::MintLiquidity::new(
        contract,
        contract.pool_id(),
        token(9),
        vec![token(1), token(2)],
        action::ShareShape::Proportional,
    )
    .expect("mint");
    assert_eq!(
        optimize(&problem(
            &[(1, 10)],
            &[(9, 1)],
            vec![Edge {
                action: action::Action::MintLiquidity(wide.clone()),
                curve: Curve::Chords(vec![Chord {
                    width: z(10),
                    output: z(7),
                    action: action::Action::MintLiquidity(wide),
                }]),
            }],
            vec![vec![0]],
        )),
        Err(OptimizeError::Wide)
    );
}

#[test]
fn a_single_token_mint_quotes_like_its_chords() {
    let contract = action::Address::new([8; 20]).expect("contract");
    let mint = action::MintLiquidity::new(
        contract,
        contract.pool_id(),
        token(9),
        vec![token(1)],
        action::ShareShape::Single,
    )
    .expect("mint");
    let problem = problem(
        &[(1, 10)],
        &[(9, 1)],
        vec![Edge {
            action: action::Action::MintLiquidity(mint.clone()),
            curve: Curve::Chords(vec![Chord {
                width: z(10),
                output: z(7),
                action: action::Action::MintLiquidity(mint),
            }]),
        }],
        vec![vec![0]],
    );
    let plan = optimize(&problem).expect("mint");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(7));
    assert_eq!(received(&plan, 9), z(7));
}

#[test]
fn a_rate_wrap_uses_its_chords() {
    let rate = action::RateWrap::new(token(2), token(1)).expect("rate");
    let problem = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![Edge {
            action: action::Action::RateWrap(rate),
            curve: Curve::Chords(vec![Chord {
                width: z(10),
                output: z(7),
                action: action::Action::RateWrap(rate),
            }]),
        }],
        vec![vec![0]],
    );
    let plan = optimize(&problem).expect("rate");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(7));
    assert_eq!(received(&plan, 2), z(7));
}

#[test]
fn small_graphs_match_the_allocation_oracle() {
    let shapes = [
        vec![(4_i64, 4_i64)],
        vec![(3, 5), (3, 1)],
        vec![(2, 3), (4, 2)],
        vec![(5, 2)],
    ];
    let gases = [(0_i64, 0_i64), (1, 4), (5, 1), (3, 3)];
    for budget in 1..=6 {
        for left in &shapes {
            for right in &shapes {
                for (gas_left, gas_right) in gases {
                    let mut sample = problem(
                        &[(1, budget)],
                        &[(2, 1)],
                        vec![
                            conduit(1, 2, shape_segments(left, 1)),
                            conduit(1, 2, shape_segments(right, 2)),
                        ],
                        vec![vec![0], vec![1]],
                    );
                    charge(&mut sample, &[(1, gas_left), (2, gas_right)]);
                    assert_oracle(&sample);

                    let mut pair = problem(
                        &[(1, budget)],
                        &[(2, 1), (3, 1)],
                        vec![
                            conduit(1, 2, shape_segments(left, 1)),
                            conduit(1, 3, shape_segments(right, 2)),
                        ],
                        vec![vec![0], vec![1]],
                    );
                    charge(&mut pair, &[(1, gas_left), (2, gas_right)]);
                    assert_oracle(&pair);
                }
            }
        }
    }
    let path_shapes = [shapes[0].clone(), shapes[1].clone(), shapes[3].clone()];
    for budget in 1..=5 {
        for direct in &path_shapes {
            for first in &path_shapes {
                for second in &path_shapes {
                    for gases in [[0, 0, 0], [4, 1, 1], [1, 3, 3]] {
                        let mut sample = problem(
                            &[(1, budget)],
                            &[(3, 1)],
                            vec![
                                conduit(1, 3, shape_segments(direct, 1)),
                                conduit(1, 2, shape_segments(first, 2)),
                                conduit(2, 3, shape_segments(second, 3)),
                            ],
                            vec![vec![0], vec![1, 2]],
                        );
                        charge(&mut sample, &[(1, gases[0]), (2, gases[1]), (3, gases[2])]);
                        assert_oracle(&sample);

                        let mut reversed = sample.clone();
                        reversed.routes =
                            vec![Route { edges: vec![1, 2] }, Route { edges: vec![0] }];
                        assert_oracle(&reversed);
                    }
                }
            }
        }
    }
    for budget in 1..=5 {
        for shared in &path_shapes {
            for left in &path_shapes {
                for right in &path_shapes {
                    let sample = problem(
                        &[(1, budget)],
                        &[(3, 1), (4, 1)],
                        vec![
                            conduit(1, 2, shape_segments(shared, 1)),
                            conduit(2, 3, shape_segments(left, 2)),
                            conduit(2, 4, shape_segments(right, 3)),
                        ],
                        vec![vec![0, 1], vec![0, 2]],
                    );
                    assert_oracle(&sample);
                }
            }
        }
    }
}

#[test]
fn the_searcher_mesh_matches_the_allocation_oracle() {
    let hub = search_token(9);
    let query = prepare(search_trade(&[(1, 8)], &[2, 3]), &[hub], None).expect("trade");
    let catalog = Catalog(HashMap::from([
        ((search_token(1), hub), vec![segment(8, 8, 1)]),
        ((hub, search_token(2)), vec![segment(8, 6, 2)]),
        ((hub, search_token(3)), vec![segment(8, 8, 3)]),
        ((search_token(1), search_token(2)), vec![segment(8, 3, 4)]),
        ((search_token(1), search_token(3)), vec![segment(8, 4, 5)]),
    ]));
    let set = execute(&query, &catalog);
    let mut sample = from_route_set(&set);
    charge(&mut sample, &[(1, 2), (2, 2), (3, 1), (4, 5), (5, 1)]);
    let plan = optimize(&sample).expect("route set");
    funded(&sample, &plan);
    let (count, _) = oracle(&sample).expect("five edges");
    assert_eq!(plan.count, z(count));
}

fn charge(problem: &mut Problem, costs: &[(u8, i64)]) {
    for (pool, gas) in costs {
        if *gas > 0 {
            set_gas(problem, *pool, *gas);
        }
    }
}

fn shape_segments(shape: &[(i64, i64)], pool: u8) -> Vec<SlopeSegment> {
    shape
        .iter()
        .map(|(width, output)| segment(*width, *output, pool))
        .collect()
}

fn assert_oracle(problem: &Problem) {
    let plan = optimize(problem).expect("curves");
    funded(problem, &plan);
    let Some((count, _)) = oracle(problem) else {
        return;
    };
    let routes: Vec<&Vec<usize>> = problem.routes.iter().map(|route| &route.edges).collect();
    assert_eq!(
        plan.count,
        z(count),
        "count inputs {:?} gas {:?} routes {:?}",
        problem.inputs,
        problem.gas,
        routes
    );
}

fn oracle(problem: &Problem) -> Option<(i64, i64)> {
    let caps: Vec<i64> = problem.edges.iter().map(edge_cap).collect();
    let mut product = 1_i64;
    for cap in &caps {
        product = product.saturating_mul(cap + 1);
        if product > 250_000 {
            return None;
        }
    }
    let mut tenders = vec![0_i64; caps.len()];
    let mut best: Option<(i64, i64)> = None;
    loop {
        if let Some(score) = score(problem, &tenders) {
            best = Some(match best {
                Some((count, gas)) if count > score.0 || (count == score.0 && gas <= score.1) => {
                    (count, gas)
                }
                Some(_) => score,
                None => score,
            });
        }
        let mut digit = 0;
        loop {
            if digit == caps.len() {
                return best;
            }
            if tenders[digit] < caps[digit] {
                tenders[digit] += 1;
                break;
            }
            tenders[digit] = 0;
            digit += 1;
        }
    }
}

fn edge_cap(edge: &Edge) -> i64 {
    use num_traits::ToPrimitive;
    match &edge.curve {
        Curve::Unlimited => 0,
        Curve::Chords(chords) => chords
            .iter()
            .map(|chord| chord.width.to_i64().unwrap_or(0))
            .sum(),
    }
}

fn score(problem: &Problem, tenders: &[i64]) -> Option<(i64, i64)> {
    use num_traits::ToPrimitive;
    let mut incoming: HashMap<Token, i64> = HashMap::new();
    let mut outgoing: HashMap<Token, i64> = HashMap::new();
    let mut gas = 0_i64;
    let mut opened = BTreeSet::new();
    for (index, edge) in problem.edges.iter().enumerate() {
        let take = tenders[index];
        if take == 0 {
            continue;
        }
        let (Some(token_in), Some(token_out)) = (edge.token_in(), edge.token_out()) else {
            return None;
        };
        *outgoing.entry(token_in).or_default() += take;
        let produced = crate::quote_curve(&edge.curve, &z(take))
            .to_i64()
            .unwrap_or(0);
        *incoming.entry(token_out).or_default() += produced;
        if opened.insert(edge.action.clone()) {
            gas += problem
                .gas
                .get(&edge.action)
                .and_then(ToPrimitive::to_i64)
                .unwrap_or(0);
        }
    }
    if let Some(ceiling) = &problem.gas_ceiling {
        if gas > ceiling.to_i64().unwrap_or(0) {
            return None;
        }
    }
    for input in &problem.inputs {
        let spend = outgoing.get(&input.token).copied().unwrap_or(0);
        if spend > input.amount.to_i64().unwrap_or(0) {
            return None;
        }
    }
    let mut count = i64::MAX;
    for output in &problem.outputs {
        let produced = incoming.get(&output.token).copied().unwrap_or(0);
        let consumed = outgoing.get(&output.token).copied().unwrap_or(0);
        if consumed > produced {
            return None;
        }
        let weight = output.weight.to_i64().unwrap_or(1);
        count = count.min((produced - consumed) / weight);
    }
    for (token, consumed) in &outgoing {
        if problem.inputs.iter().any(|input| input.token == *token) {
            continue;
        }
        if problem.outputs.iter().any(|output| output.token == *token) {
            continue;
        }
        let produced = incoming.get(token).copied().unwrap_or(0);
        if *consumed > produced {
            return None;
        }
    }
    if count == i64::MAX {
        count = 0;
    }
    Some((count, gas))
}

fn cp_curve(reserve_in: i64, reserve_out: i64, points: &[i64], pool: u8) -> Vec<SlopeSegment> {
    let knots: Vec<(BigInt, BigInt)> = points
        .iter()
        .map(|dx| {
            let out = (reserve_out * dx) / (reserve_in + dx);
            (z(*dx), z(out))
        })
        .collect();
    routing::segments_from_knots(&knots, pool_key(pool))
}

fn search_token(byte: u8) -> searcher::Token {
    searcher::Token([byte; 20])
}

fn search_trade(inputs: &[(u8, u64)], outputs: &[u8]) -> Trade {
    Trade {
        inputs: inputs
            .iter()
            .map(|(byte, amount)| (search_token(*byte), BigUint::from(*amount)))
            .collect(),
        outputs: outputs.iter().copied().map(search_token).collect(),
    }
}

struct Catalog(HashMap<(searcher::Token, searcher::Token), Vec<SlopeSegment>>);

impl Conduits for Catalog {
    fn conduit(
        &self,
        token_in: searcher::Token,
        token_out: searcher::Token,
    ) -> Option<Vec<SlopeSegment>> {
        self.0.get(&(token_in, token_out)).cloned()
    }
}

fn amount(text: &str) -> BigInt {
    text.parse().expect("amount")
}

fn wide_chord(token_in: u8, token_out: u8, width: &str, output: &str, pool: u8) -> Edge {
    Edge {
        action: swap_action(token(token_in), token(token_out), pool),
        curve: Curve::Chords(vec![Chord {
            width: amount(width),
            output: amount(output),
            action: swap_action(token(token_in), token(token_out), pool),
        }]),
    }
}

/// One chord pays two output per input. The whole budget fits on it.
#[test]
fn one_output_at_ten_to_the_twenty_one_pays_the_full_chord() {
    let budget = "1000000000000000000000";
    let problem = Problem {
        inputs: vec![Input {
            token: token(1),
            amount: amount(budget),
        }],
        outputs: vec![Output {
            token: token(2),
            weight: z(1),
        }],
        edges: vec![wide_chord(1, 2, budget, "2000000000000000000000", 1)],
        routes: vec![Route { edges: vec![0] }],
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    };
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, amount("2000000000000000000000"));
    assert_eq!(plan.flows.len(), 1);
    assert_eq!(plan.flows[0].input, amount(budget));
    assert_eq!(plan.flows[0].output, plan.count);
    assert_eq!(plan.gas, z(0));
    assert_eq!(plan.tenders.len(), 1);
    assert_eq!(plan.tenders[0].input, amount(budget));
}

/// The budget covers part of a wider chord. The pay is the truncating quote.
#[test]
fn one_output_inside_a_wide_chord_pays_the_truncating_quote() {
    let problem = Problem {
        inputs: vec![Input {
            token: token(1),
            amount: amount("1000000000000000000"),
        }],
        outputs: vec![Output {
            token: token(2),
            weight: z(1),
        }],
        edges: vec![wide_chord(
            1,
            2,
            "1000000000000000000000",
            "2000000000000000000000",
            1,
        )],
        routes: vec![Route { edges: vec![0] }],
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    };
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, amount("2000000000000000000"));
    assert_eq!(plan.flows.len(), 1);
    assert_eq!(plan.flows[0].input, amount("1000000000000000000"));
    assert_eq!(plan.flows[0].output, plan.count);
    assert_eq!(plan.gas, z(0));
}

/// Two routes share one budget. The forward bound sums both full quotes.
/// The plan spends the steeper route first and the rest on the other.
#[test]
fn one_output_with_a_shared_budget_keeps_the_exhausted_flows() {
    let problem = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![
            conduit(1, 2, vec![segment(4, 12, 1)]),
            conduit(1, 2, vec![segment(10, 10, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(18));
    assert_eq!(spent(&plan, 1), z(10));
    assert_eq!(plan.flows.len(), 2);
    assert_eq!(plan.flows[0].route, 0);
    assert_eq!(plan.flows[0].input, z(4));
    assert_eq!(plan.flows[0].output, z(12));
    assert_eq!(plan.flows[1].route, 1);
    assert_eq!(plan.flows[1].input, z(6));
    assert_eq!(plan.flows[1].output, z(6));
    assert_eq!(plan.gas, z(0));
}

/// Both legs stay on the basket pace. Count 31 needs 11 of the budget.
#[test]
fn the_basket_funds_the_last_pace_both_legs_can_meet() {
    let problem = problem(
        &[(1, 10)],
        &[(2, 1), (3, 1)],
        vec![
            conduit(1, 2, vec![segment(10, 100, 1)]),
            conduit(1, 3, vec![segment(10, 50, 2)]),
        ],
        vec![vec![0], vec![1]],
    );
    assert!(crate::solve::meet(&problem, &z(100)).is_none());
    let low = crate::solve::meet(&problem, &z(30)).expect("pace");
    funded(&problem, &low);
    assert_eq!(low.count, z(30));
    assert_eq!(spent(&low, 1), z(9));
    assert_eq!(received(&low, 2), z(30));
    assert_eq!(received(&low, 3), z(30));
    let plan = optimize(&problem).expect("basket");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(30));
    assert_eq!(spent(&plan, 1), z(9));
    assert_eq!(received(&plan, 2), z(30));
    assert_eq!(received(&plan, 3), z(30));
    assert_eq!(plan.gas, z(0));
    assert_eq!(plan.flows.len(), 2);
    assert_eq!(plan.flows[0].route, 0);
    assert_eq!(plan.flows[0].input, z(3));
    assert_eq!(plan.flows[0].output, z(30));
    assert_eq!(plan.flows[1].route, 1);
    assert_eq!(plan.flows[1].input, z(6));
    assert_eq!(plan.flows[1].output, z(30));
    assert_eq!(plan.tenders.len(), 2);
    assert_eq!(plan.tenders[0].input, z(3));
    assert_eq!(plan.tenders[1].input, z(6));
}

/// Both legs draw on one budget through a single wide chord. The pace jumps
/// by the residual output, so the fill does not walk the raw amount.
#[test]
fn a_wide_shared_budget_funds_the_basket_in_chord_steps() {
    use std::time::Instant;

    let width = "1000000000000000000";
    let problem = Problem {
        inputs: vec![Input {
            token: token(1),
            amount: amount(width),
        }],
        outputs: vec![
            Output {
                token: token(2),
                weight: z(1),
            },
            Output {
                token: token(3),
                weight: z(1),
            },
        ],
        edges: vec![
            wide_chord(1, 2, width, width, 1),
            wide_chord(1, 3, width, width, 2),
        ],
        routes: vec![Route { edges: vec![0] }, Route { edges: vec![1] }],
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    };
    crate::fund::probe::reset();
    let started = Instant::now();
    let plan = optimize(&problem).expect("basket");
    let elapsed = started.elapsed();
    funded(&problem, &plan);
    assert_eq!(plan.count, amount("500000000000000000"));
    assert!(
        crate::fund::probe::steps() <= 4,
        "steps {}",
        crate::fund::probe::steps()
    );
    assert!(elapsed.as_millis() < 1_000);
}

/// The downstream chord binds. The rank is that piece, so the path still
/// beats a shallower direct curve.
#[test]
fn a_downstream_chord_keeps_the_path_steeper_than_its_source() {
    let problem = problem(
        &[(1, 10)],
        &[(2, 1)],
        vec![
            conduit(1, 9, vec![segment(100, 100, 1)]),
            conduit(9, 2, vec![segment(10, 10, 2)]),
            conduit(1, 2, vec![segment(10, 5, 3)]),
        ],
        vec![vec![0, 1], vec![2]],
    );
    let plan = optimize(&problem).expect("path");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(10));
    assert_eq!(spent(&plan, 1), z(10));
    assert_eq!(plan.flows.len(), 1);
    assert_eq!(plan.flows[0].route, 0);
    assert_eq!(plan.flows[0].input, z(10));
    assert_eq!(plan.flows[0].output, z(10));
}

/// A later chord is not walked while the open chord can still pay.
#[test]
fn a_long_curve_funds_the_open_chord_without_walking_the_tail() {
    let chord_n = 40;
    let chords: Vec<_> = (0..chord_n).map(|_| segment(100, 100, 1)).collect();
    let problem = problem(
        &[(1, 100 * chord_n)],
        &[(2, 1)],
        vec![conduit(1, 2, chords)],
        vec![vec![0]],
    );
    crate::solve::reset_chord_visits();
    crate::fund::probe::reset();
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(100 * chord_n));
    let steps = crate::fund::probe::steps();
    let visits = crate::solve::chord_visits();
    assert_eq!(steps, chord_n as u64);
    assert!(visits <= steps * 16, "steps {steps} visits {visits}");
}

/// Ranking reads the open chord. It does not binary-search the raw amount
/// once per route per chord.
#[test]
fn a_wide_basket_ranks_each_chord_without_searching_the_amount() {
    let width = amount("1000000000000000000");
    let routes_n = 8;
    let chord_n = 3;
    let mut edges = Vec::new();
    let mut routes = Vec::new();
    for (token_in, token_out, pool_base) in [(1_u8, 3_u8, 1_u8), (2, 4, 40)] {
        for index in 0..routes_n {
            let pool = pool_base + index as u8;
            let slope = (routes_n - index) as i64;
            let action = swap_action(token(token_in), token(token_out), pool);
            let output = &width * slope;
            edges.push(Edge {
                action: action.clone(),
                curve: Curve::Chords(
                    (0..chord_n)
                        .map(|_| Chord {
                            width: width.clone(),
                            output: output.clone(),
                            action: action.clone(),
                        })
                        .collect(),
                ),
            });
            routes.push(Route {
                edges: vec![edges.len() - 1],
            });
        }
    }
    let budget = &width * (routes_n * chord_n) as i64;
    let problem = Problem {
        inputs: vec![
            Input {
                token: token(1),
                amount: budget.clone(),
            },
            Input {
                token: token(2),
                amount: budget,
            },
        ],
        outputs: vec![
            Output {
                token: token(3),
                weight: z(1),
            },
            Output {
                token: token(4),
                weight: z(1),
            },
        ],
        edges,
        routes,
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    };
    crate::fund::probe::reset();
    let plan = optimize(&problem).expect("basket");
    funded(&problem, &plan);
    // 3 chords on slopes 8+7+…+1, both legs funded in full.
    assert_eq!(plan.count, &width * 108);
    assert_eq!(spent(&plan, 1), &width * 24);
    assert_eq!(spent(&plan, 2), &width * 24);
    let steps = crate::fund::probe::steps();
    let prices = crate::fund::probe::prices();
    assert_eq!(steps, (routes_n * chord_n) as u64);
    // One joint quote-search per step, not one amount-search per route.
    assert!(prices <= steps * 80, "steps {steps} prices {prices}");
}

/// Forty direct routes. The budget covers two full chords and half of the next.
#[test]
fn the_steepest_of_many_routes_is_filled_first() {
    let mut edges = Vec::new();
    let mut routes = Vec::new();
    for index in 0..40_u8 {
        edges.push(conduit(
            1,
            2,
            vec![segment(10, 1000 - i64::from(index), index + 1)],
        ));
        routes.push(vec![usize::from(index)]);
    }
    let problem = problem(&[(1, 25)], &[(2, 1)], edges, routes);
    let plan = optimize(&problem).expect("wide");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(2498));
    assert_eq!(spent(&plan, 1), z(25));
    assert_eq!(plan.flows.len(), 3);
    assert_eq!(plan.flows[0].route, 0);
    assert_eq!(plan.flows[0].input, z(10));
    assert_eq!(plan.flows[0].output, z(1000));
    assert_eq!(plan.flows[1].route, 1);
    assert_eq!(plan.flows[1].input, z(10));
    assert_eq!(plan.flows[1].output, z(999));
    assert_eq!(plan.flows[2].route, 2);
    assert_eq!(plan.flows[2].input, z(5));
    assert_eq!(plan.flows[2].output, z(499));
    assert_eq!(plan.gas, z(0));
}

/// Both slopes fit in `u128` and their product does not. The steeper chord wins.
#[test]
fn a_wide_slope_product_prefers_the_steeper_chord() {
    let width = "1000000000000000000000";
    let problem = Problem {
        inputs: vec![Input {
            token: token(1),
            amount: amount(width),
        }],
        outputs: vec![Output {
            token: token(2),
            weight: z(1),
        }],
        edges: vec![
            wide_chord(1, 2, width, "2000000000000000000000", 1),
            wide_chord(1, 2, width, "3000000000000000000000", 2),
        ],
        routes: vec![Route { edges: vec![0] }, Route { edges: vec![1] }],
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    };
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(plan.count, amount("3000000000000000000000"));
    assert_eq!(plan.flows.len(), 1);
    assert_eq!(plan.flows[0].route, 1);
    assert_eq!(plan.flows[0].input, amount(width));
}

/// A chord past `u128` uses the general quote and the same steeper-first order.
#[test]
fn a_slope_past_u128_prefers_the_steeper_chord() {
    let width = "10000000000000000000000000000000000000000";
    let problem = Problem {
        inputs: vec![Input {
            token: token(1),
            amount: amount(width),
        }],
        outputs: vec![Output {
            token: token(2),
            weight: z(1),
        }],
        edges: vec![
            wide_chord(1, 2, width, "20000000000000000000000000000000000000000", 1),
            wide_chord(1, 2, width, "30000000000000000000000000000000000000000", 2),
        ],
        routes: vec![Route { edges: vec![0] }, Route { edges: vec![1] }],
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    };
    let plan = optimize(&problem).expect("curve");
    funded(&problem, &plan);
    assert_eq!(
        plan.count,
        amount("30000000000000000000000000000000000000000")
    );
    assert_eq!(plan.flows.len(), 1);
    assert_eq!(plan.flows[0].route, 1);
}

fn from_route_set(set: &searcher::RouteSet) -> Problem {
    let mut edges = Vec::new();
    for edge in &set.edges {
        let token_in = edge.token_in;
        let token_out = edge.token_out;
        match &edge.curve {
            SearchCurve::Wrap => edges.push(custody_edge(token_in, token_out)),
            SearchCurve::Conduit(segments) => {
                let chords: Vec<Chord> = segments
                    .iter()
                    .map(|segment| Chord {
                        width: segment.width.clone(),
                        output: segment.output.clone(),
                        action: swap_action(token_in, token_out, segment.action.pool[0]),
                    })
                    .collect();
                let action = chords
                    .first()
                    .map(|chord| chord.action.clone())
                    .unwrap_or_else(|| swap_action(token_in, token_out, 1));
                edges.push(Edge {
                    action,
                    curve: Curve::Chords(chords),
                });
            }
        }
    }
    Problem {
        inputs: set
            .inputs
            .iter()
            .map(|(token, amount)| Input {
                token: *token,
                amount: BigInt::from(amount.clone()),
            })
            .collect(),
        outputs: set
            .outputs
            .iter()
            .map(|token| Output {
                token: *token,
                weight: z(1),
            })
            .collect(),
        edges,
        routes: set
            .routes
            .iter()
            .map(|route| Route {
                edges: route.edges.clone(),
            })
            .collect(),
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    }
}
