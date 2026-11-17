//! Diagnostic value of one allocated plan.
//!
//! The figures are printed after the allocator returns. Search and the
//! allocator do not read them.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use num_bigint::BigInt;
use num_traits::Zero;
use searcher::Token;
use simulate::{Price, Ratio};

use crate::token::{self, Known};

/// Wei paid for one gas unit when the survey prices gas.
///
/// The allocator does not read this. Native ETH is one raw numeraire unit per wei.
pub(super) const WEI_PER_GAS: u64 = 300_000_000;

/// A marked gain above this fraction of the input value is a surplus.
const SURPLUS_NUMER: i64 = 101;
const SURPLUS_DENOM: i64 = 100;

/// Keeping less than this fraction of the input value is a large loss.
const LOSS_NUMER: i64 = 1;
const LOSS_DENOM: i64 = 2;

pub(super) struct Report {
    pub lines: Vec<String>,
    pub impact: Option<Ratio>,
    pub cumulative: Ratio,
    pub surplus: bool,
    pub large_loss: bool,
    pub routes: Vec<(Ratio, String)>,
}

pub(super) struct Quote<'a> {
    pub prices: &'a BTreeMap<Token, Price>,
    pub known: &'a [Known],
    pub places: &'a BTreeMap<Token, u8>,
    pub decimals: u32,
    pub wei_per_gas: u64,
}

pub(super) fn report(
    problem: &optimizer::Problem,
    plan: &optimizer::Plan,
    scored: &simulate::Score,
    quote: &Quote<'_>,
) -> Report {
    let gas = simulate::gas_value(&plan.gas, quote.wei_per_gas);
    let cumulative = simulate::cumulative(&scored.value_out, &gas);
    let impact = scored.retained.as_ref().map(simulate::impact);
    let mut surplus = scored
        .retained
        .as_ref()
        .is_some_and(|retained| above(retained, SURPLUS_NUMER, SURPLUS_DENOM));
    let mut large_loss = scored
        .retained
        .as_ref()
        .is_some_and(|retained| below(retained, LOSS_NUMER, LOSS_DENOM));
    let mut marked = Vec::new();
    for flow in &plan.flows {
        if flow.input.is_zero() && flow.output.is_zero() {
            continue;
        }
        let path = path_tokens(problem, flow.route);
        let route = simulate::mark_route(&path, &flow.input, &flow.output, quote.prices);
        if let (Some(value_in), Some(value_out)) = (&route.value_in, &route.value_out) {
            if richer(value_out, value_in) {
                surplus = true;
            }
            if let Some(loss) = &route.loss {
                if lost_half(loss, value_in) {
                    large_loss = true;
                }
            }
        }
        let text = route_line(flow.route, &route, quote);
        marked.push((route.loss, flow.route, text));
    }
    marked.sort_by(|left, right| match (&left.0, &right.0) {
        (Some(left_loss), Some(right_loss)) => right_loss
            .cmp_value(left_loss)
            .then_with(|| left.1.cmp(&right.1)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => left.1.cmp(&right.1),
    });
    let mut lines = vec![
        format!(
            "value_out {}",
            simulate::whole_tokens(&scored.value_out, quote.decimals, 6)
        ),
        format!("gas_wei {}", quote.wei_per_gas),
        format!(
            "gas_value {}",
            simulate::whole_tokens(&gas, quote.decimals, 6)
        ),
        format!(
            "cumulative {}",
            simulate::whole_tokens(&cumulative, quote.decimals, 6)
        ),
    ];
    if let Some(impact) = &impact {
        lines.push(format!("impact {}", simulate::decimal(impact, 6)));
    }
    let mut routes = Vec::new();
    for (loss, _, text) in marked {
        lines.push(text.clone());
        if let Some(loss) = loss {
            routes.push((loss, text));
        }
    }
    if surplus {
        lines.push("suspect surplus".to_string());
    }
    if large_loss {
        lines.push("suspect loss".to_string());
    }
    Report {
        lines,
        impact,
        cumulative,
        surplus,
        large_loss,
        routes,
    }
}

pub(super) fn path_tokens(problem: &optimizer::Problem, route: usize) -> Vec<Token> {
    let Some(route) = problem.routes.get(route) else {
        return Vec::new();
    };
    let mut tokens = Vec::new();
    for (index, edge_index) in route.edges.iter().enumerate() {
        let Some(edge) = problem.edges.get(*edge_index) else {
            continue;
        };
        if index == 0 {
            if let Some(token) = edge.token_in() {
                tokens.push(token);
            }
        }
        if let Some(token) = edge.token_out() {
            tokens.push(token);
        }
    }
    tokens
}

fn route_line(index: usize, route: &simulate::MarkedRoute, quote: &Quote<'_>) -> String {
    let spots = route
        .spots
        .iter()
        .map(|(token, price)| {
            let name = token::label(*token, quote.known);
            match price {
                Some(price) => {
                    let scale = u32::from(quote.places.get(token).copied().unwrap_or(18));
                    format!("{name}={}", whole_price(price, scale, quote.decimals))
                }
                None => format!("{name}=unpriced"),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    match (&route.loss, &route.value_in, &route.value_out) {
        (Some(loss), Some(value_in), Some(value_out)) => format!(
            "route {index} loss {} in {} out {} {spots}",
            simulate::whole_tokens(loss, quote.decimals, 6),
            simulate::whole_tokens(value_in, quote.decimals, 6),
            simulate::whole_tokens(value_out, quote.decimals, 6),
        ),
        _ => format!("route {index} unpriced {spots}"),
    }
}

fn whole_price(price: &Price, token_decimals: u32, numeraire_decimals: u32) -> String {
    let numer = &price.numer * pow10(token_decimals);
    let denom = &price.denom * pow10(numeraire_decimals);
    simulate::decimal(&Ratio::new(numer, denom), 6)
}

fn above(ratio: &Ratio, numer: i64, denom: i64) -> bool {
    ratio.cmp_value(&Ratio::new(BigInt::from(numer), BigInt::from(denom))) == Ordering::Greater
}

fn below(ratio: &Ratio, numer: i64, denom: i64) -> bool {
    ratio.cmp_value(&Ratio::new(BigInt::from(numer), BigInt::from(denom))) == Ordering::Less
}

fn richer(value_out: &Ratio, value_in: &Ratio) -> bool {
    &value_out.numer * &value_in.denom * BigInt::from(SURPLUS_DENOM)
        > &value_in.numer * &value_out.denom * BigInt::from(SURPLUS_NUMER)
}

fn lost_half(loss: &Ratio, value_in: &Ratio) -> bool {
    &loss.numer * &value_in.denom * BigInt::from(LOSS_DENOM)
        > &value_in.numer * &loss.denom * BigInt::from(LOSS_NUMER)
}

fn pow10(exp: u32) -> BigInt {
    let mut value = BigInt::from(1);
    let ten = BigInt::from(10);
    for _ in 0..exp {
        value *= &ten;
    }
    value
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use action::{Action, Address, Invariant, Swap, Token};
    use num_bigint::BigInt;
    use optimizer::{Edge, Flow, Input, Output, Plan, Problem, Route};
    use simulate::{Price, Ratio};

    use super::{report, Quote};

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn price(numer: i64, denom: i64) -> Price {
        Price {
            numer: BigInt::from(numer),
            denom: BigInt::from(denom),
        }
    }

    fn swap(pool: u8, token_in: Token, token_out: Token) -> Action {
        let contract = Address::new([pool; 20]).expect("contract");
        Action::Swap(
            Swap::new(
                contract,
                contract.pool_id(),
                token_in,
                token_out,
                Invariant::Product,
            )
            .expect("swap"),
        )
    }

    fn plan(flow_in: i64, flow_out: i64) -> (Problem, Plan) {
        let source = token(1);
        let middle = token(2);
        let sink = token(3);
        let first = swap(1, source, middle);
        let second = swap(2, middle, sink);
        let problem = Problem {
            inputs: vec![Input {
                token: source,
                amount: BigInt::from(flow_in),
            }],
            outputs: vec![Output {
                token: sink,
                weight: BigInt::from(1),
            }],
            edges: vec![
                Edge {
                    action: first,
                    curve: optimizer::Curve::Unlimited,
                },
                Edge {
                    action: second,
                    curve: optimizer::Curve::Unlimited,
                },
            ],
            routes: vec![Route { edges: vec![0, 1] }],
            hubs: Vec::new(),
            gas: BTreeMap::new(),
            gas_ceiling: None,
        };
        let plan = Plan {
            count: BigInt::from(1),
            flows: vec![Flow {
                route: 0,
                input: BigInt::from(flow_in),
                output: BigInt::from(flow_out),
            }],
            spent: vec![(source, BigInt::from(flow_in))],
            received: vec![(sink, BigInt::from(flow_out))],
            tenders: Vec::new(),
            gas: BigInt::from(210_000),
        };
        (problem, plan)
    }

    #[test]
    fn the_report_lists_output_value_gas_cost_and_cumulative_apart() {
        let (problem, plan) = plan(5_000_000_000_000_000_000, 2_000_000_000_000_000_000);
        let mut prices = BTreeMap::new();
        prices.insert(token(1), price(1, 1));
        prices.insert(token(3), price(1, 1));
        let scored = simulate::score(
            &[simulate::Leg {
                token: token(1),
                amount: BigInt::from(5_000_000_000_000_000_000_i64),
            }],
            &[simulate::Leg {
                token: token(3),
                amount: BigInt::from(2_000_000_000_000_000_000_i64),
            }],
            &prices,
            &plan.gas,
        );
        let mut places = BTreeMap::new();
        places.insert(token(1), 18);
        places.insert(token(2), 18);
        places.insert(token(3), 18);
        let reported = report(
            &problem,
            &plan,
            &scored,
            &Quote {
                prices: &prices,
                known: &[],
                places: &places,
                decimals: 18,
                wei_per_gas: 300_000_000,
            },
        );
        assert!(reported
            .lines
            .iter()
            .any(|line| line == "value_out 2.000000"));
        assert!(reported
            .lines
            .iter()
            .any(|line| line == "gas_value 0.000063"));
        assert!(reported
            .lines
            .iter()
            .any(|line| line == "cumulative 1.999937"));
        assert_eq!(
            scored.retained,
            Some(Ratio::new(BigInt::from(2), BigInt::from(5)))
        );
        assert!(reported.large_loss);
        let route = reported
            .lines
            .iter()
            .find(|line| line.starts_with("route "))
            .expect("route");
        assert!(route.contains("loss 3.000000"));
        assert!(route.contains("0x0101010101010101010101010101010101010101=1.000000"));
        assert!(route.contains("0x0202020202020202020202020202020202020202=unpriced"));
        assert!(route.contains("0x0303030303030303030303030303030303030303=1.000000"));
    }

    #[test]
    fn a_gain_above_one_percent_of_input_value_is_a_surplus() {
        let (problem, plan) = plan(1_000_000_000_000_000_000, 2_000_000_000_000_000_000);
        let mut prices = BTreeMap::new();
        prices.insert(token(1), price(1, 1));
        prices.insert(token(3), price(1, 1));
        let scored = simulate::score(
            &[simulate::Leg {
                token: token(1),
                amount: BigInt::from(1_000_000_000_000_000_000_i64),
            }],
            &[simulate::Leg {
                token: token(3),
                amount: BigInt::from(2_000_000_000_000_000_000_i64),
            }],
            &prices,
            &plan.gas,
        );
        let places = BTreeMap::new();
        let reported = report(
            &problem,
            &plan,
            &scored,
            &Quote {
                prices: &prices,
                known: &[],
                places: &places,
                decimals: 18,
                wei_per_gas: 300_000_000,
            },
        );
        assert!(reported.surplus);
        assert!(!reported.large_loss);
    }
}
