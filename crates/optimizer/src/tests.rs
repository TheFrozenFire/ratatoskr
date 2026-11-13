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

fn chords_of(segments: &[SlopeSegment]) -> Vec<Chord> {
    segments
        .iter()
        .map(|segment| Chord {
            width: segment.width.clone(),
            output: segment.output.clone(),
        })
        .collect()
}

fn conduit(token_in: u8, token_out: u8, segments: Vec<SlopeSegment>) -> Edge {
    let pool = segments
        .first()
        .map(|segment| segment.action.pool[0])
        .unwrap_or(1);
    assert!(
        segments
            .iter()
            .all(|segment| segment.action.pool[0] == pool),
        "one edge holds one action"
    );
    Edge {
        action: swap_action(token(token_in), token(token_out), pool),
        curve: Curve::Chords(chords_of(&segments)),
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
    let keys: Vec<action::Action> = problem
        .edges
        .iter()
        .filter(|edge| is_pool(&edge.action, pool))
        .map(|edge| edge.action.clone())
        .collect();
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
fn an_output_that_is_also_an_intermediate_keeps_its_own_amount() {
    let problem = problem(
        &[(1, 10)],
        &[(2, 1), (3, 1)],
        vec![
            conduit(1, 2, vec![segment(10, 10, 1)]),
            conduit(2, 3, vec![segment(10, 10, 2)]),
        ],
        vec![vec![0], vec![0, 1]],
    );
    let plan = optimize(&problem).expect("curves");
    funded(&problem, &plan);
    assert_eq!(plan.count, z(5));
    assert!(received(&plan, 2) >= z(5));
    assert!(received(&plan, 3) >= z(5));
    assert_eq!(spent(&plan, 1), z(10));
}

#[test]
fn equal_baskets_keep_the_smaller_gas() {
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
    assert_eq!(plan.gas, z(1));
    assert_eq!(plan.tenders.len(), 1);
    assert!(is_pool(&plan.tenders[0].action, 2));
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

fn settled(problem: &Problem, plan: &crate::Plan) {
    let mut owners = BTreeSet::new();
    for edge in &problem.edges {
        if !owners.insert(edge.action.clone()) {
            return;
        }
    }
    let mut incoming: HashMap<Token, BigInt> = HashMap::new();
    let mut outgoing: HashMap<Token, BigInt> = HashMap::new();
    for edge in &problem.edges {
        let Some((token_in, token_out)) = edge.token_in().zip(edge.token_out()) else {
            return;
        };
        let input = plan
            .tenders
            .iter()
            .find(|tender| tender.action == edge.action)
            .map(|tender| tender.input.clone())
            .unwrap_or_else(BigInt::zero);
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
fn equal_counts_leave_the_steep_action_closed() {
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
    assert_eq!(plan.gas, z(2));
    assert!(plan
        .tenders
        .iter()
        .all(|tender| !is_pool(&tender.action, 1)));
}

#[test]
fn one_action_funds_the_count_for_less_gas_than_two_slices() {
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
    assert_eq!(plan.gas, z(4));
    assert_eq!(plan.tenders.len(), 1);
    assert!(is_pool(&plan.tenders[0].action, 3));
}

#[test]
fn the_tail_action_funds_the_count_when_the_prefix_is_closed() {
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
    assert_eq!(plan.gas, z(1));
    assert_eq!(plan.tenders.len(), 1);
    assert!(is_pool(&plan.tenders[0].action, 2));
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
                action: action::Action::MintLiquidity(wide),
                curve: Curve::Chords(vec![Chord {
                    width: z(10),
                    output: z(7),
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
            action: action::Action::MintLiquidity(mint),
            curve: Curve::Chords(vec![Chord {
                width: z(10),
                output: z(7),
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
    let (count, gas) = oracle(&sample).expect("five edges");
    assert_eq!(plan.count, z(count));
    assert_eq!(plan.gas, z(gas));
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
    let Some((count, gas)) = oracle(problem) else {
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
    assert_eq!(
        plan.gas,
        z(gas),
        "gas inputs {:?} gas {:?} routes {:?}",
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

fn group_by_pool(segments: &[SlopeSegment]) -> Vec<(u8, Vec<SlopeSegment>)> {
    let mut order = Vec::new();
    let mut groups: BTreeMap<routing::Action, Vec<SlopeSegment>> = BTreeMap::new();
    for segment in segments {
        if !groups.contains_key(&segment.action) {
            order.push(segment.action);
        }
        groups
            .entry(segment.action)
            .or_default()
            .push(segment.clone());
    }
    order
        .into_iter()
        .filter_map(|key| {
            let group = groups.remove(&key)?;
            Some((key.pool[0], group))
        })
        .collect()
}

fn from_route_set(set: &searcher::RouteSet) -> Problem {
    let mut edges = Vec::new();
    let mut expanded = Vec::new();
    for edge in &set.edges {
        let token_in = Token(edge.token_in.0);
        let token_out = Token(edge.token_out.0);
        let start = edges.len();
        match &edge.curve {
            SearchCurve::Wrap => edges.push(custody_edge(token_in, token_out)),
            SearchCurve::Conduit(segments) => {
                let groups = group_by_pool(segments);
                if groups.is_empty() {
                    edges.push(Edge {
                        action: swap_action(token_in, token_out, 1),
                        curve: Curve::Chords(Vec::new()),
                    });
                } else {
                    for (pool, group) in groups {
                        edges.push(Edge {
                            action: swap_action(token_in, token_out, pool),
                            curve: Curve::Chords(chords_of(&group)),
                        });
                    }
                }
            }
        }
        expanded.push((start..edges.len()).collect::<Vec<_>>());
    }
    let mut routes = Vec::new();
    for route in &set.routes {
        let mut choices = vec![Vec::new()];
        for index in &route.edges {
            let mut grown = Vec::new();
            for prefix in &choices {
                for edge_index in &expanded[*index] {
                    let mut path = prefix.clone();
                    path.push(*edge_index);
                    grown.push(path);
                }
            }
            choices = grown;
        }
        for path in choices {
            routes.push(Route { edges: path });
        }
    }
    Problem {
        inputs: set
            .inputs
            .iter()
            .map(|(token, amount)| Input {
                token: Token(token.0),
                amount: BigInt::from(amount.clone()),
            })
            .collect(),
        outputs: set
            .outputs
            .iter()
            .map(|token| Output {
                token: Token(token.0),
                weight: z(1),
            })
            .collect(),
        edges,
        routes,
        hubs: Vec::new(),
        gas: BTreeMap::new(),
        gas_ceiling: None,
    }
}
