use std::collections::{HashMap, HashSet};

use num_bigint::BigUint;
use searcher::{Token, NATIVE};

use super::{defined, path_ends, single, Class, Scenario};
use crate::graph::{Deep, Graph, PathEnds};
use crate::kinds::kind_list;
use crate::token::{amount, label, Known};

pub(crate) fn via_scenarios(
    via: &[PathEnds],
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (index, path) in via.iter().enumerate() {
        let Some((from, to)) = path_ends(&path.path) else {
            continue;
        };
        let mut scenario = single(
            format!("via-{index:02}"),
            format!(
                "shortest catalog path is 2 conduits through {}, kinds {}, min width {}",
                path.path
                    .get(1)
                    .map(|hub| label(*hub, known))
                    .unwrap_or_else(|| "-".into()),
                kind_list(path.kinds),
                path.min_capacity
            ),
            from,
            to,
            BigUint::from(1_u32),
            hubs,
            wrapped,
        );
        scenario.sweep = (index == 0).then(|| "via-0".to_string());
        scenario.class = Class::Via;
        scenario.paths = vec![path.path.clone()];
        scenario.show_path = true;
        scenario.exact_path = true;
        out.push(scenario);
    }
    if let Some(path) = via.first() {
        let Some((from, to)) = path_ends(&path.path) else {
            return out;
        };
        let mut scenario = single(
            "via-00-ether",
            "same one-hub pair at 10^18 raw units".to_string(),
            from,
            to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.sweep = Some("via-0".to_string());
        scenario.class = Class::Via;
        scenario.paths = vec![path.path.clone()];
        scenario.show_path = true;
        scenario.exact_path = true;
        out.push(scenario);
    }
    out
}

pub(crate) fn deep_scenarios(
    graph: &Graph,
    deep: &[Deep],
    toward_hub: bool,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    let mut native_at = HashSet::new();
    for item in deep {
        let hub_name = label(item.hub, known);
        let (name, detail, from, to) = if toward_hub {
            (
                format!("deep-to-{hub_name}-d{:02}", item.dist),
                format!(
                    "shortest catalog path to {hub_name} is {} conduits, min width {}, kinds {}",
                    item.dist,
                    item.min_capacity,
                    kind_list(item.kinds)
                ),
                item.token,
                item.hub,
            )
        } else {
            (
                format!("deep-from-{hub_name}-d{:02}", item.dist),
                format!(
                    "shortest catalog path from {hub_name} is {} conduits, min width {}, kinds {}",
                    item.dist,
                    item.min_capacity,
                    kind_list(item.kinds)
                ),
                item.hub,
                item.token,
            )
        };
        let mut scenario = single(name, detail, from, to, BigUint::from(1_u32), hubs, wrapped);
        scenario.class = Class::Deep;
        scenario.paths = vec![item.path.clone()];
        scenario.show_path = true;
        scenario.cover = true;
        out.push(scenario);
        if item.hub == wrapped && native_at.insert(item.dist) {
            let (name, from, to, detail) = if toward_hub {
                (
                    format!("deep-to-native-d{:02}", item.dist),
                    item.token,
                    NATIVE,
                    format!(
                        "native ETH out, catalog path to {} is {} conduits",
                        label(wrapped, known),
                        item.dist
                    ),
                )
            } else {
                (
                    format!("native-deep-d{:02}", item.dist),
                    NATIVE,
                    item.token,
                    format!(
                        "native ETH in, catalog path from {} is {} conduits",
                        label(wrapped, known),
                        item.dist
                    ),
                )
            };
            if graph.meta(from, to).is_some() {
                continue;
            }
            let mut scenario = single(name, detail, from, to, amount(18), hubs, wrapped);
            scenario.class = Class::Deep;
            scenario.paths = vec![item.path.clone()];
            scenario.show_path = true;
            scenario.cover = true;
            out.push(scenario);
        }
    }
    out
}

pub(crate) fn long_scenarios(long: &[PathEnds], hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (index, path) in long.iter().enumerate() {
        let Some((from, to)) = path_ends(&path.path) else {
            continue;
        };
        let conduits = path.path.len().saturating_sub(1);
        let cross = if path.cross_hub {
            " The path uses a conduit between two hubs."
        } else {
            ""
        };
        let through_native = if path.path.contains(&NATIVE) {
            " The shortest path uses native ETH."
        } else {
            ""
        };
        let mut scenario = single(
            format!("long-{conduits:02}-{index:02}"),
            format!(
                "shortest catalog path is {conduits} conduits. Minimum conduit width is {}. Tendril kinds are {}. Endpoint pools are disjoint.{cross}{through_native}",
                path.min_capacity,
                kind_list(path.tendril_kinds)
            ),
            from,
            to,
            BigUint::from(1_u32),
            hubs,
            wrapped,
        );
        scenario.class = Class::Long;
        scenario.paths = vec![path.path.clone()];
        scenario.show_path = true;
        scenario.cover = true;
        out.push(scenario);
        if index < 3 && path.min_capacity > 1 {
            let mut spent = single(
                format!("long-spend-{conduits:02}-{index:02}"),
                format!(
                    "same ends, input amount is the minimum conduit width {}",
                    path.min_capacity
                ),
                from,
                to,
                BigUint::from(path.min_capacity),
                hubs,
                wrapped,
            );
            spent.paths = vec![path.path.clone()];
            spent.show_path = true;
            spent.cover = true;
            out.push(spent);
        }
    }
    out
}

pub(crate) fn multi_scenarios(
    long: &[PathEnds],
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (index, group) in group_by(long, true).into_iter().take(4).enumerate() {
        push_multi(&mut out, &group, true, index, known, hubs, wrapped);
    }
    for (index, group) in group_by(long, false).into_iter().take(4).enumerate() {
        push_multi(&mut out, &group, false, index, known, hubs, wrapped);
    }
    out
}

pub(crate) fn push_multi(
    out: &mut Vec<Scenario>,
    group: &[&PathEnds],
    shared_dest: bool,
    index: usize,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) {
    let paths: Vec<Vec<Token>> = group.iter().take(3).map(|path| path.path.clone()).collect();
    if paths.len() < 2 {
        return;
    }
    let amounts = [
        BigUint::from(1_u32),
        BigUint::from(1_000_000_u32),
        amount(18),
    ];
    let scenario = if shared_dest {
        let dest = *paths[0].last().expect("dest");
        let inputs = paths
            .iter()
            .enumerate()
            .map(|(nth, path)| (path[0], amounts[nth].clone()))
            .collect();
        Scenario {
            name: format!("long-in-{}-{index:02}", paths.len()),
            detail: format!(
                "{} low-liquidity inputs, one output {}, each catalog path is longer than two hubs",
                paths.len(),
                label(dest, known)
            ),
            trade: defined(inputs, vec![dest], wrapped),
            hubs: hubs.to_vec(),
            paths,
            show_path: true,
            direct_kinds: None,
            expect_pure: Vec::new(),
            must_empty: false,
            expect_routes: None,
            sweep: None,
            require_end: None,
            forbid_ends: Vec::new(),
            reach: Vec::new(),
            must_wrap: false,
            cover: true,
            exact_path: false,
            class: Class::Long,
        }
    } else {
        let source = paths[0][0];
        let outputs = paths
            .iter()
            .filter_map(|path| path.last().copied())
            .collect();
        Scenario {
            name: format!("long-out-{}-{index:02}", paths.len()),
            detail: format!(
                "{} in, {} low-liquidity outputs, each catalog path is longer than two hubs",
                label(source, known),
                paths.len()
            ),
            trade: defined(vec![(source, amount(18))], outputs, wrapped),
            hubs: hubs.to_vec(),
            paths,
            show_path: true,
            direct_kinds: None,
            expect_pure: Vec::new(),
            must_empty: false,
            expect_routes: None,
            sweep: None,
            require_end: None,
            forbid_ends: Vec::new(),
            reach: Vec::new(),
            must_wrap: false,
            cover: true,
            exact_path: false,
            class: Class::Long,
        }
    };
    out.push(scenario);
}

pub(crate) fn group_by(paths: &[PathEnds], by_dest: bool) -> Vec<Vec<&PathEnds>> {
    let mut order = Vec::new();
    let mut groups: HashMap<Token, Vec<&PathEnds>> = HashMap::new();
    for path in paths {
        let Some(key) = (if by_dest {
            path.path.last()
        } else {
            path.path.first()
        })
        .copied() else {
            continue;
        };
        if !groups.contains_key(&key) {
            order.push(key);
        }
        groups.entry(key).or_default().push(path);
    }
    let mut found = Vec::new();
    for key in order {
        let Some(group) = groups.remove(&key) else {
            continue;
        };
        let mut seen = HashSet::new();
        let distinct: Vec<&PathEnds> = group
            .into_iter()
            .filter(|path| {
                let mark = if by_dest {
                    path.path.first().copied()
                } else {
                    path.path.last().copied()
                };
                mark.map(|token| seen.insert(token)).unwrap_or(false)
            })
            .collect();
        if distinct.len() >= 2 {
            found.push(distinct);
        }
    }
    found
}

pub(crate) fn isolated_scenarios(
    isolated: &[PathEnds],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (index, path) in isolated.iter().enumerate() {
        let Some((from, to)) = path_ends(&path.path) else {
            continue;
        };
        let conduits = path.path.len().saturating_sub(1);
        let mut scenario = single(
            format!("isolated-{index:02}"),
            format!(
                "component apart from the hubs, shortest path is {conduits} conduits, kinds {}",
                kind_list(path.kinds)
            ),
            from,
            to,
            BigUint::from(1_u32),
            hubs,
            wrapped,
        );
        scenario.class = Class::Isolated;
        scenario.paths = vec![path.path.clone()];
        scenario.show_path = true;
        scenario.cover = true;
        out.push(scenario);
    }
    out
}
