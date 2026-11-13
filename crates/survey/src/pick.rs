use std::collections::{HashMap, HashSet};

use searcher::{Token, NATIVE};

use crate::graph::{self, Graph};
use crate::kinds::kind_bit;
use crate::scan::{Bag, Example, Survey};

pub(crate) struct Picks {
    pub(crate) witness: Vec<(u8, Example)>,
    pub(crate) rich: Vec<(u8, Example)>,
    pub(crate) into_weth: Vec<(u8, Example)>,
    pub(crate) out_of_weth: Vec<(u8, Example)>,
    pub(crate) mixed: Vec<(u8, Example)>,
}

pub(crate) fn picks(
    survey: &Survey,
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    weth: Token,
) -> Picks {
    let mut kinds: Vec<u8> = survey.pure.keys().copied().collect();
    kinds.sort_unstable();
    let mut witness = Vec::new();
    let mut rich = Vec::new();
    for kind in &kinds {
        let Some(bag) = survey.pure.get(kind) else {
            continue;
        };
        let Some(chosen) = choose_witness(bag, hubs) else {
            continue;
        };
        if let Some(largest) = bag.high.first() {
            if largest.from != chosen.from || largest.to != chosen.to {
                rich.push((*kind, largest.clone()));
            }
        }
        witness.push((*kind, chosen));
    }
    let into_weth = bundle_into(survey, graph, incoming, hubs, weth);
    let out_of_weth = bundle_out(survey, graph, incoming, hubs, weth);
    let mut mixed = Vec::new();
    for kind in 1..15_u8 {
        if survey.pure.contains_key(&kind) {
            continue;
        }
        if let Some(example) = survey.carrier.get(&kind) {
            if example.kinds & kind_bit(kind) != 0 {
                mixed.push((kind, example.clone()));
            }
        }
    }
    Picks {
        witness,
        rich,
        into_weth,
        out_of_weth,
        mixed,
    }
}

pub(crate) fn choose_witness(bag: &Bag, hubs: &HashSet<Token>) -> Option<Example> {
    let mut ranked: Vec<&Example> = bag.low.iter().chain(bag.high.iter()).collect();
    ranked.sort_by(|left, right| {
        let left_leaf = u32::from(!leaf(left, hubs));
        let right_leaf = u32::from(!leaf(right, hubs));
        left_leaf
            .cmp(&right_leaf)
            .then(left.capacity.cmp(&right.capacity))
            .then(left.from.cmp(&right.from))
    });
    ranked.first().map(|example| (*example).clone())
}

pub(crate) fn leaf(example: &Example, hubs: &HashSet<Token>) -> bool {
    example.from != NATIVE
        && example.to != NATIVE
        && !hubs.contains(&example.from)
        && !hubs.contains(&example.to)
}

pub(crate) fn bundle_into(
    survey: &Survey,
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    weth: Token,
) -> Vec<(u8, Example)> {
    let exact = gather_into(survey, weth, |example| {
        sole(graph, incoming, example.from, hubs) == Some(weth)
    });
    if !exact.is_empty() {
        return exact;
    }
    gather_into(survey, weth, |_| true)
}

pub(crate) fn gather_into(
    survey: &Survey,
    weth: Token,
    pred: impl Fn(&Example) -> bool,
) -> Vec<(u8, Example)> {
    let mut kinds: Vec<u8> = survey.pure.keys().copied().collect();
    kinds.sort_unstable();
    let mut used = HashSet::new();
    let mut found = Vec::new();
    for kind in kinds {
        let Some(bag) = survey.pure.get(&kind) else {
            continue;
        };
        let Some(example) = bag.low.iter().chain(bag.high.iter()).find(|example| {
            example.to == weth
                && example.from != NATIVE
                && example.from != weth
                && !used.contains(&example.from)
                && pred(example)
        }) else {
            continue;
        };
        used.insert(example.from);
        found.push((kind, example.clone()));
    }
    found
}

pub(crate) fn bundle_out(
    survey: &Survey,
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    weth: Token,
) -> Vec<(u8, Example)> {
    let exact = gather_out(survey, graph, incoming, hubs, weth, true);
    if !exact.is_empty() {
        return exact;
    }
    gather_out(survey, graph, incoming, hubs, weth, false)
}

pub(crate) fn gather_out(
    survey: &Survey,
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    weth: Token,
    exact: bool,
) -> Vec<(u8, Example)> {
    let mut kinds: Vec<u8> = survey.pure.keys().copied().collect();
    kinds.sort_unstable();
    let mut used = HashSet::new();
    let mut found = Vec::new();
    for kind in kinds {
        let Some(bag) = survey.pure.get(&kind) else {
            continue;
        };
        let Some(example) = bag.low.iter().chain(bag.high.iter()).find(|example| {
            example.from == weth
                && example.to != NATIVE
                && !hubs.contains(&example.to)
                && !used.contains(&example.to)
                && (!exact || sole(graph, incoming, example.to, hubs) == Some(weth))
        }) else {
            continue;
        };
        used.insert(example.to);
        found.push((kind, example.clone()));
    }
    found
}

pub(crate) fn sole(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    token: Token,
    hubs: &HashSet<Token>,
) -> Option<Token> {
    graph::sole_hub(graph, incoming, token, hubs)
}
