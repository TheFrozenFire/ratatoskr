use std::collections::HashSet;

use num_bigint::BigUint;
use searcher::{Token, NATIVE};
use venues::kind_name;

use super::{defined, multi, single, Class, Scenario};
use crate::graph::{Deep, PathEnds};
use crate::kinds::kind_bit;
use crate::pick::Picks;
use crate::scan::Survey;
use crate::token::{amount, label, Known};

pub(crate) fn width_scenarios(
    survey: &Survey,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut widths: Vec<u32> = survey.by_width.keys().copied().collect();
    widths.sort_unstable();
    let mut out = Vec::new();
    for width in widths {
        let Some(example) = survey.by_width.get(&width) else {
            continue;
        };
        let mut scenario = single(
            format!("mix-{width:02}"),
            format!(
                "direct conduit spans {width} venue kinds, {} -> {}, width {}",
                label(example.from, known),
                label(example.to, known),
                example.capacity
            ),
            example.from,
            example.to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.direct_kinds = Some(example.kinds);
        out.push(scenario);
    }
    out
}

pub(crate) fn kind_scenarios(
    picks: &Picks,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    for (kind, example) in &picks.witness {
        let mut scenario = single(
            format!("kind-{}", kind_name(*kind)),
            format!(
                "pure {} conduit {} -> {} width {} segments {}",
                kind_name(*kind),
                label(example.from, known),
                label(example.to, known),
                example.capacity,
                example.segments
            ),
            example.from,
            example.to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.direct_kinds = Some(kind_bit(*kind));
        scenario.sweep = Some(format!("kind-{}", kind_name(*kind)));
        out.push(scenario);
        let mut wei = single(
            format!("kind-{}-wei", kind_name(*kind)),
            format!("same pure {} conduit at 1 raw unit", kind_name(*kind)),
            example.from,
            example.to,
            BigUint::from(1_u32),
            hubs,
            wrapped,
        );
        wei.direct_kinds = Some(kind_bit(*kind));
        wei.sweep = Some(format!("kind-{}", kind_name(*kind)));
        out.push(wei);
    }
    for (kind, example) in &picks.rich {
        let mut scenario = single(
            format!("kind-rich-{}", kind_name(*kind)),
            format!(
                "largest pure {} conduit {} -> {} width {} segments {}",
                kind_name(*kind),
                label(example.from, known),
                label(example.to, known),
                example.capacity,
                example.segments
            ),
            example.from,
            example.to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.direct_kinds = Some(kind_bit(*kind));
        out.push(scenario);
    }
    for (kind, example) in &picks.mixed {
        let mut scenario = single(
            format!("kind-mixed-{}", kind_name(*kind)),
            format!(
                "{} appears inside a {}-kind conduit {} -> {}",
                kind_name(*kind),
                example.kinds.count_ones(),
                label(example.from, known),
                label(example.to, known)
            ),
            example.from,
            example.to,
            amount(18),
            hubs,
            wrapped,
        );
        scenario.direct_kinds = Some(example.kinds);
        out.push(scenario);
    }
    out
}

pub(crate) fn native_kind_scenarios(
    survey: &Survey,
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut kinds: Vec<u8> = survey.pure.keys().copied().collect();
    kinds.sort_unstable();
    let mut out = Vec::new();
    let ether = amount(18);
    for kind in kinds {
        let Some(bag) = survey.pure.get(&kind) else {
            continue;
        };
        let mut examples = bag.low.iter().chain(bag.high.iter());
        if let Some(example) = examples
            .clone()
            .find(|example| example.from == wrapped && !hubs.contains(&example.to))
        {
            let mut scenario = single(
                format!("native-out-{}", kind_name(kind)),
                format!(
                    "native ETH in, {} out, through a pure {} conduit",
                    label(example.to, known),
                    kind_name(kind)
                ),
                NATIVE,
                example.to,
                ether.clone(),
                hubs,
                wrapped,
            );
            scenario.must_wrap = true;
            scenario.expect_pure = vec![(wrapped, example.to, kind_bit(kind))];
            out.push(scenario);
        }
        if let Some(example) = examples.find(|example| {
            example.to == wrapped && example.from != wrapped && !hubs.contains(&example.from)
        }) {
            let mut scenario = single(
                format!("native-in-{}", kind_name(kind)),
                format!(
                    "{} in, native ETH out, through a pure {} conduit",
                    label(example.from, known),
                    kind_name(kind)
                ),
                example.from,
                NATIVE,
                ether.clone(),
                hubs,
                wrapped,
            );
            scenario.must_wrap = true;
            scenario.expect_pure = vec![(example.from, wrapped, kind_bit(kind))];
            out.push(scenario);
        }
    }
    out
}

pub(crate) fn split_scenarios(
    via: &[PathEnds],
    deep: &[Deep],
    known: &[Known],
    hubs: &[Token],
    wrapped: Token,
) -> Vec<Scenario> {
    let mut out = Vec::new();
    let mut used = HashSet::new();
    for (index, path) in via.iter().enumerate() {
        if out.len() == 8 {
            break;
        }
        let Some(from) = path.path.first().copied() else {
            continue;
        };
        let Some(hub) = path.path.get(1).copied() else {
            continue;
        };
        let far = deep.iter().find(|item| {
            item.dist >= 2
                && item.hub == hub
                && item.token != from
                && item.token != hub
                && !used.contains(&item.token)
        });
        let Some(far) = far else {
            continue;
        };
        used.insert(far.token);
        let name = format!("split-{index:02}");
        let detail = format!(
            "{} spends one input. The outputs are {} and {}, and {} is {} conduits downstream of {}.",
            label(from, known),
            label(hub, known),
            label(far.token, known),
            label(far.token, known),
            far.dist,
            label(far.hub, known)
        );
        let mut scenario = multi(
            &name,
            &detail,
            vec![(from, BigUint::from(1_u32))],
            vec![hub, far.token],
            hubs,
            wrapped,
        );
        scenario.reach = vec![hub, far.token];
        scenario.paths = vec![path.path.clone(), far.path.clone()];
        scenario.show_path = true;
        out.push(scenario);
    }
    out
}

pub(crate) fn bundle_scenarios(picks: &Picks, hubs: &[Token], wrapped: Token) -> Vec<Scenario> {
    let mut out = Vec::new();
    if picks.into_weth.len() >= 2 {
        let inputs = picks
            .into_weth
            .iter()
            .map(|(_, example)| (example.from, amount(6)))
            .collect();
        let expect_pure = picks
            .into_weth
            .iter()
            .map(|(kind, example)| (example.from, example.to, kind_bit(*kind)))
            .collect();
        let names = picks
            .into_weth
            .iter()
            .map(|(kind, _)| kind_name(*kind))
            .collect::<Vec<_>>()
            .join(",");
        out.push(Scenario {
            name: "kinds-into-weth".into(),
            detail: format!(
                "{} pure conduits into WETH, one input each, amount 10^6 ({names})",
                picks.into_weth.len()
            ),
            trade: defined(inputs, vec![wrapped], wrapped),
            hubs: hubs.to_vec(),
            paths: Vec::new(),
            show_path: false,
            direct_kinds: None,
            expect_pure,
            must_empty: false,
            expect_routes: None,
            sweep: None,
            require_end: None,
            forbid_ends: Vec::new(),
            reach: Vec::new(),
            must_wrap: false,
            cover: false,
            exact_path: false,
            class: Class::Ordinary,
        });
    }
    if picks.out_of_weth.len() >= 2 {
        let outputs = picks
            .out_of_weth
            .iter()
            .map(|(_, example)| example.to)
            .collect();
        let expect_pure = picks
            .out_of_weth
            .iter()
            .map(|(kind, example)| (example.from, example.to, kind_bit(*kind)))
            .collect();
        let names = picks
            .out_of_weth
            .iter()
            .map(|(kind, _)| kind_name(*kind))
            .collect::<Vec<_>>()
            .join(",");
        out.push(Scenario {
            name: "kinds-out-of-weth".into(),
            detail: format!(
                "WETH in, {} pure conduit outputs ({names})",
                picks.out_of_weth.len()
            ),
            trade: defined(vec![(wrapped, amount(18))], outputs, wrapped),
            hubs: hubs.to_vec(),
            paths: Vec::new(),
            show_path: false,
            direct_kinds: None,
            expect_pure,
            must_empty: false,
            expect_routes: None,
            sweep: None,
            require_end: None,
            forbid_ends: Vec::new(),
            reach: Vec::new(),
            must_wrap: false,
            cover: false,
            exact_path: false,
            class: Class::Ordinary,
        });
    }
    out
}
