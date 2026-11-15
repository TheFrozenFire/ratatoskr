//! The action table for one allocated plan.

use std::collections::{BTreeMap, BTreeSet};

use action::Action;
use num_bigint::BigInt;
use num_traits::Zero;
use searcher::Token;

use crate::token;

pub(super) fn allocation_lines(
    problem: &optimizer::Problem,
    plan: &optimizer::Plan,
    known: &[crate::token::Known],
) -> Vec<String> {
    let mut unwrap_in = BigInt::from(0);
    let mut swap_source = BigInt::from(0);
    let mut others = Vec::new();
    for tender in &plan.tenders {
        if matches!(tender.action, Action::Wrap(_) | Action::Unwrap(_)) {
            unwrap_in += &tender.input;
        } else {
            others.push(tender);
        }
    }
    for flow in &plan.flows {
        let lone_custodial = problem.routes.get(flow.route).is_some_and(|route| {
            route.edges.len() == 1
                && problem
                    .edges
                    .get(route.edges[0])
                    .is_some_and(|edge| matches!(edge.action, Action::Wrap(_) | Action::Unwrap(_)))
        });
        if !lone_custodial {
            swap_source += &flow.input;
        }
    }
    others.sort_by(|left, right| right.input.cmp(&left.input));
    let mut lines = vec![
        format!("unwrap_in {unwrap_in}"),
        format!("swap_source {swap_source}"),
        format!("tenders {}", plan.tenders.len()),
    ];
    for tender in others.iter().take(8) {
        lines.push(format!(
            "tender {} {} {} {}",
            action_kind(&tender.action),
            token::label(tender.token_in, known),
            token::label(tender.token_out, known),
            tender.input
        ));
    }
    let mut by_len = BTreeMap::<usize, (usize, BigInt, BigInt)>::new();
    for flow in &plan.flows {
        let len = problem
            .routes
            .get(flow.route)
            .map(|route| route.edges.len())
            .unwrap_or(0);
        let entry = by_len
            .entry(len)
            .or_insert_with(|| (0, BigInt::from(0), BigInt::from(0)));
        entry.0 += 1;
        entry.1 += &flow.input;
        entry.2 += &flow.output;
    }
    let flows = by_len
        .iter()
        .map(|(len, (count, input, output))| format!("{len}:{count}:{input}:{output}"))
        .collect::<Vec<_>>()
        .join(",");
    lines.push(format!("flows {flows}"));
    lines
}

/// One consumed slope segment. `output` is that segment's integer share of the edge quote.
struct ActionRow {
    kind: &'static str,
    contract: String,
    pool: String,
    token_in: Token,
    token_out: Token,
    input: BigInt,
    output: BigInt,
    width: BigInt,
    chord_output: BigInt,
}

pub(super) fn action_table(
    problem: &optimizer::Problem,
    plan: &optimizer::Plan,
    known: &[crate::token::Known],
    places: &BTreeMap<Token, u8>,
) -> Vec<String> {
    let mut rows = Vec::new();
    for edge in &problem.edges {
        let (Some(token_in), Some(token_out)) = (edge.token_in(), edge.token_out()) else {
            continue;
        };
        let input = edge_input(edge, &plan.tenders, token_in, token_out);
        if input <= BigInt::from(0) {
            continue;
        }
        rows.extend(edge_rows(edge, token_in, token_out, &input));
    }
    rows.sort_by(|left, right| right.output.cmp(&left.output));
    let mut lines = vec![format!("action_rows {}", rows.len()), action_header()];
    for row in rows {
        lines.push(action_line(&row, known, places));
    }
    lines
}

fn action_header() -> String {
    [
        "action",
        "kind",
        "contract",
        "pool",
        "token_in",
        "token_out",
        "dec_in",
        "dec_out",
        "input",
        "output",
        "width",
        "chord_output",
        "in_units",
        "out_units",
        "out_per_in",
    ]
    .join("\t")
}

fn action_line(
    row: &ActionRow,
    known: &[crate::token::Known],
    places: &BTreeMap<Token, u8>,
) -> String {
    let dec_in = places.get(&row.token_in).copied();
    let dec_out = places.get(&row.token_out).copied();
    [
        "action".to_string(),
        row.kind.to_string(),
        row.contract.clone(),
        row.pool.clone(),
        token::label(row.token_in, known),
        token::label(row.token_out, known),
        dec_in
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into()),
        dec_out
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into()),
        row.input.to_string(),
        row.output.to_string(),
        row.width.to_string(),
        row.chord_output.to_string(),
        units(&row.input, dec_in),
        units(&row.output, dec_out),
        out_per_in(&row.output, &row.input, dec_out, dec_in),
    ]
    .join("\t")
}

fn edge_input(
    edge: &optimizer::Edge,
    tenders: &[optimizer::Tender],
    token_in: Token,
    token_out: Token,
) -> BigInt {
    let mut owned = BTreeSet::new();
    match &edge.curve {
        optimizer::Curve::Unlimited => {
            owned.insert(edge.action.clone());
        }
        optimizer::Curve::Chords(chords) => {
            for chord in chords {
                owned.insert(chord.action.clone());
            }
        }
    }
    let mut input = BigInt::from(0);
    for tender in tenders {
        if tender.token_in == token_in
            && tender.token_out == token_out
            && owned.contains(&tender.action)
        {
            input += &tender.input;
        }
    }
    input
}

fn edge_rows(
    edge: &optimizer::Edge,
    token_in: Token,
    token_out: Token,
    input: &BigInt,
) -> Vec<ActionRow> {
    match &edge.curve {
        optimizer::Curve::Unlimited => vec![ActionRow {
            kind: action_kind(&edge.action),
            contract: locate(&edge.action).0,
            pool: locate(&edge.action).1,
            token_in,
            token_out,
            input: input.clone(),
            output: input.clone(),
            width: BigInt::from(0),
            chord_output: BigInt::from(0),
        }],
        optimizer::Curve::Chords(chords) => {
            let mut left = input.clone();
            let mut rows = Vec::new();
            for chord in chords {
                if left.is_zero() || chord.width.is_zero() {
                    break;
                }
                let taken = if left < chord.width {
                    left.clone()
                } else {
                    chord.width.clone()
                };
                let paid = (&chord.output * &taken) / &chord.width;
                let (contract, pool) = locate(&chord.action);
                rows.push(ActionRow {
                    kind: action_kind(&chord.action),
                    contract,
                    pool,
                    token_in,
                    token_out,
                    input: taken.clone(),
                    output: paid,
                    width: chord.width.clone(),
                    chord_output: chord.output.clone(),
                });
                left -= taken;
            }
            rows
        }
    }
}

fn locate(action: &Action) -> (String, String) {
    match action {
        Action::Swap(inner) => (hex_bytes(&inner.contract.0), hex_bytes(&inner.pool)),
        Action::MintLiquidity(inner) => (hex_bytes(&inner.contract.0), hex_bytes(&inner.pool)),
        Action::BurnLiquidity(inner) => (hex_bytes(&inner.contract.0), hex_bytes(&inner.pool)),
        _ => ("-".into(), "-".into()),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    let mut out = String::from("0x");
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

fn units(amount: &BigInt, decimals: Option<u8>) -> String {
    let Some(decimals) = decimals else {
        return "-".into();
    };
    let scale = ten(decimals);
    let whole = amount / &scale;
    let frac = (amount % &scale).to_string();
    if decimals == 0 {
        return whole.to_string();
    }
    format!("{whole}.{:0>width$}", frac, width = decimals as usize)
}

/// Output token units paid for one input token unit.
fn out_per_in(output: &BigInt, input: &BigInt, dec_out: Option<u8>, dec_in: Option<u8>) -> String {
    let (Some(dec_out), Some(dec_in)) = (dec_out, dec_in) else {
        return "-".into();
    };
    if input.is_zero() {
        return "-".into();
    }
    let numer = output * ten(dec_in);
    let denom = input * ten(dec_out);
    let places = 8_u8;
    let scale = ten(places);
    let scaled = &numer * &scale / &denom;
    let whole = &scaled / &scale;
    let frac = (&scaled % &scale).to_string();
    format!("{whole}.{:0>width$}", frac, width = places as usize)
}

fn ten(decimals: u8) -> BigInt {
    let mut value = BigInt::from(1);
    for _ in 0..decimals {
        value *= 10;
    }
    value
}

fn action_kind(action: &Action) -> &'static str {
    match action {
        Action::Wrap(_) => "wrap",
        Action::Unwrap(_) => "unwrap",
        Action::RateWrap(_) => "rate-wrap",
        Action::RateUnwrap(_) => "rate-unwrap",
        Action::Swap(_) => "swap",
        Action::MintLiquidity(_) => "mint",
        Action::BurnLiquidity(_) => "burn",
        Action::DepositVault(_) => "deposit",
        Action::RedeemVault(_) => "redeem",
        Action::Supply(_) => "supply",
        Action::Withdraw(_) => "withdraw",
        Action::Stake(_) => "stake",
        Action::Unstake(_) => "unstake",
        Action::Convert(_) => "convert",
        Action::Fill(_) => "fill",
        Action::Split(_) => "split",
        Action::Merge(_) => "merge",
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use action::{Action, Address, Invariant, Swap, Token};
    use num_bigint::BigInt;
    use optimizer::{Chord, Curve, Edge, Input, Output, Plan, Problem, Route, Tender};

    use super::action_table;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
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

    #[test]
    fn each_consumed_segment_keeps_its_share_of_the_quote() {
        let token_in = token(1);
        let token_out = token(2);
        let steep = swap(1, token_in, token_out);
        let flat = swap(2, token_in, token_out);
        let problem = Problem {
            inputs: vec![Input {
                token: token_in,
                amount: BigInt::from(6),
            }],
            outputs: vec![Output {
                token: token_out,
                weight: BigInt::from(1),
            }],
            edges: vec![Edge {
                action: steep.clone(),
                curve: Curve::Chords(vec![
                    Chord {
                        width: BigInt::from(4),
                        output: BigInt::from(10),
                        action: steep.clone(),
                    },
                    Chord {
                        width: BigInt::from(4),
                        output: BigInt::from(2),
                        action: flat.clone(),
                    },
                ]),
            }],
            routes: vec![Route { edges: vec![0] }],
            hubs: Vec::new(),
            gas: BTreeMap::new(),
            gas_ceiling: None,
        };
        let plan = Plan {
            count: BigInt::from(11),
            flows: Vec::new(),
            spent: Vec::new(),
            received: Vec::new(),
            tenders: vec![
                Tender {
                    action: steep,
                    token_in,
                    token_out,
                    input: BigInt::from(4),
                },
                Tender {
                    action: flat,
                    token_in,
                    token_out,
                    input: BigInt::from(2),
                },
            ],
            gas: BigInt::from(0),
        };
        let mut places = BTreeMap::new();
        places.insert(token_in, 0);
        places.insert(token_out, 0);
        let lines = action_table(&problem, &plan, &[], &places);
        assert_eq!(lines[0], "action_rows 2");
        assert!(lines[1].starts_with("action\tkind\t"));
        assert!(lines[2].contains("\t4\t10\t4\t10\t4\t10\t2.50000000"));
        assert!(lines[3].contains("\t2\t1\t4\t2\t2\t1\t0.50000000"));
    }
}
