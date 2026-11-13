use std::collections::{BTreeMap, HashMap, HashSet};

use searcher::{Curve, Route, RouteSet, Token, NATIVE};
use venues::kind_name;

use crate::graph::{Graph, Selection};
use crate::kinds::{curve_kinds, kind_bit, kind_list, only_kind};
use crate::report::shapes;
use crate::scan::Survey;
use crate::scenario::{Class, Scenario};
use crate::token::{hex, label, Known};

pub(crate) struct Checks {
    pub(crate) failures: Vec<String>,
    pub(crate) kinds: u32,
    pub(crate) exact_pure: HashSet<u8>,
    pub(crate) saw_wrap: bool,
    pub(crate) saw_one: bool,
    pub(crate) saw_two: bool,
    pub(crate) sweep: HashMap<String, String>,
    pub(crate) long: usize,
    pub(crate) via: usize,
    pub(crate) deep: usize,
    pub(crate) long_lengths: BTreeMap<usize, usize>,
}

pub(crate) fn route_end(set: &RouteSet, route: &Route) -> Option<Token> {
    let index = *route.edges.last()?;
    set.edges.get(index).map(|edge| edge.token_out)
}

pub(crate) fn record(
    checks: &mut Checks,
    scenario: &Scenario,
    set: &RouteSet,
    known: &[Known],
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
) {
    for edge in &set.edges {
        let bits = curve_kinds(&edge.curve);
        checks.kinds |= bits;
        if let Some(kind) = only_kind(bits) {
            checks.exact_pure.insert(kind);
        }
        if matches!(edge.curve, Curve::Wrap) {
            checks.saw_wrap = true;
        }
    }
    let shapes = shapes(set);
    checks.saw_one |= shapes.one > 0;
    checks.saw_two |= shapes.two > 0;
    if scenario.must_empty && !set.routes.is_empty() {
        checks.failures.push(format!(
            "{} returned {} routes",
            scenario.name,
            set.routes.len()
        ));
    }
    if let Some(want) = scenario.expect_routes {
        if set.routes.len() != want {
            checks.failures.push(format!(
                "{} routes {} want {want}",
                scenario.name,
                set.routes.len()
            ));
        }
    }
    if scenario.cover {
        for path in &scenario.paths {
            if !path_covered(scenario, set, path) {
                checks.failures.push(format!(
                    "{} has no route across its catalog path of {} conduits ({})",
                    scenario.name,
                    path.len().saturating_sub(1),
                    path_degrees(graph, incoming, &scenario.hubs, path, known)
                ));
            }
        }
    }
    if scenario.exact_path {
        let matched = scenario.paths.iter().any(|path| {
            set.routes
                .iter()
                .any(|route| route_tokens(set, route) == path.as_slice())
        });
        if !matched {
            checks.failures.push(format!(
                "{} does not contain its catalog path",
                scenario.name
            ));
        }
    }
    for token in &scenario.reach {
        let reached = set
            .routes
            .iter()
            .any(|route| route_end(set, route) == Some(*token));
        if !reached {
            checks.failures.push(format!(
                "{} never reaches {}",
                scenario.name,
                label(*token, known)
            ));
        }
    }
    if let Some(expected) = scenario.direct_kinds {
        let from = scenario.trade.inputs[0].0;
        let to = scenario.trade.outputs[0].0;
        match edge_kinds(set, from, to) {
            Some(found) if found == expected => {}
            Some(found) => checks.failures.push(format!(
                "{} direct kinds {} want {}",
                scenario.name,
                kind_list(found),
                kind_list(expected)
            )),
            None => checks
                .failures
                .push(format!("{} missing direct conduit", scenario.name)),
        }
    }
    for (from, to, kinds) in &scenario.expect_pure {
        match edge_kinds(set, *from, *to) {
            Some(found) if found == *kinds => {}
            Some(found) => checks.failures.push(format!(
                "{} {} -> {} kinds {} want {}",
                scenario.name,
                label(*from, known),
                label(*to, known),
                kind_list(found),
                kind_list(*kinds)
            )),
            None => checks.failures.push(format!(
                "{} missing {} -> {}",
                scenario.name,
                label(*from, known),
                label(*to, known)
            )),
        }
    }
    if let Some(token) = scenario.require_end {
        let reached = set
            .routes
            .iter()
            .any(|route| route_end(set, route) == Some(token));
        if !reached {
            checks.failures.push(format!(
                "{} never reaches {}",
                scenario.name,
                label(token, known)
            ));
        }
    }
    for token in &scenario.forbid_ends {
        if set
            .routes
            .iter()
            .any(|route| route_end(set, route) == Some(*token))
        {
            checks.failures.push(format!(
                "{} reaches {}",
                scenario.name,
                label(*token, known)
            ));
        }
    }
    if scenario.must_wrap
        && !set
            .edges
            .iter()
            .any(|edge| matches!(edge.curve, Curve::Wrap))
    {
        checks
            .failures
            .push(format!("{} has no wrap", scenario.name));
    }
    if let Some(group) = &scenario.sweep {
        let signature = topology(set);
        if let Some(previous) = checks.sweep.get(group) {
            if previous != &signature {
                checks
                    .failures
                    .push(format!("{} amount changed the route set", scenario.name));
            }
        } else {
            checks.sweep.insert(group.clone(), signature);
        }
    }
    match scenario.class {
        Class::Long if scenario.paths.len() == 1 => {
            checks.long += 1;
            let conduits = scenario.paths[0].len().saturating_sub(1);
            *checks.long_lengths.entry(conduits).or_default() += 1;
        }
        Class::Via => checks.via += 1,
        Class::Deep => checks.deep += 1,
        _ => {}
    }
}

pub(crate) fn finish_checks(checks: &mut Checks, survey: &Survey, selection: &Selection) {
    for kind in 1..15_u8 {
        if checks.kinds & kind_bit(kind) == 0 {
            checks.failures.push(format!(
                "venue kind {} is absent from every route set",
                kind_name(kind)
            ));
        }
    }
    let mut pure: Vec<u8> = survey.pure.keys().copied().collect();
    pure.sort_unstable();
    for kind in pure {
        if !checks.exact_pure.contains(&kind) {
            checks.failures.push(format!(
                "pure {} never appears as its own conduit",
                kind_name(kind)
            ));
        }
    }
    if !checks.saw_wrap {
        checks.failures.push("no route uses the wrap".into());
    }
    if !checks.saw_one {
        checks.failures.push("no one-hub route".into());
    }
    if !checks.saw_two {
        checks.failures.push("no two-hub route".into());
    }
    if checks.via == 0 {
        checks.failures.push("no one-hub leaf pair".into());
    }
    if checks.long == 0 {
        checks
            .failures
            .push("no long memecoin pair survived the pool check".into());
    }
    if selection.max_to_hub_small >= 2 && selection.deep_to.is_empty() {
        checks.failures.push(
            "small tokens sit two or more conduits from a hub, and no deep scenario was built"
                .into(),
        );
    }
    if selection.max_from_hub_small >= 2 && selection.deep_from.is_empty() {
        checks.failures.push(
            "small tokens sit two or more conduits out from a hub, and no deep scenario was built"
                .into(),
        );
    }
}

pub(crate) fn push_checks(out: &mut String, checks: &Checks) {
    out.push_str("-- checks\n");
    out.push_str(&format!(
        "kinds {}\nexact pure {}\nlong paths {} lengths {}\nvia {} deep {}\nwrap {} one-hub {} two-hub {}\n",
        kind_list(checks.kinds),
        checks.exact_pure.len(),
        checks.long,
        checks
            .long_lengths
            .iter()
            .map(|(len, count)| format!("{len}:{count}"))
            .collect::<Vec<_>>()
            .join(" "),
        checks.via,
        checks.deep,
        checks.saw_wrap,
        checks.saw_one,
        checks.saw_two
    ));
    if checks.failures.is_empty() {
        out.push_str("ok\n");
    } else {
        for failure in &checks.failures {
            out.push_str("fail ");
            out.push_str(failure);
            out.push('\n');
        }
    }
}

pub(crate) fn path_degrees(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &[Token],
    path: &[Token],
    known: &[Known],
) -> String {
    path.iter()
        .map(|token| {
            let indeg = incoming.get(token).map(|preds| preds.len()).unwrap_or(0);
            let mark = if hubs.contains(token) { "h" } else { "" };
            format!(
                "{}:{}/{}{mark}",
                label(*token, known),
                graph.degree(*token),
                indeg
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn route_tokens(set: &RouteSet, route: &Route) -> Vec<Token> {
    let Some(first) = route.edges.first() else {
        return Vec::new();
    };
    let mut tokens = vec![set.edges[*first].token_in];
    for index in &route.edges {
        if let Some(edge) = set.edges.get(*index) {
            tokens.push(edge.token_out);
        }
    }
    tokens
}

pub(crate) fn path_covered(scenario: &Scenario, set: &RouteSet, path: &[Token]) -> bool {
    let (Some(&catalog_start), Some(&catalog_end)) = (path.first(), path.last()) else {
        return false;
    };
    let mut start = catalog_start;
    let mut end = catalog_end;
    let mut budget = path.len().saturating_sub(1);
    if scenario
        .trade
        .inputs
        .iter()
        .any(|(token, _)| *token == NATIVE)
        && start != NATIVE
        && scenario.trade.wrapped == Some(start)
    {
        start = NATIVE;
        budget += 1;
    }
    if scenario
        .trade
        .outputs
        .iter()
        .any(|(token, _)| *token == NATIVE)
        && end != NATIVE
        && scenario.trade.wrapped == Some(end)
    {
        end = NATIVE;
        budget += 1;
    }
    set.routes.iter().any(|route| {
        if route.edges.len() > budget {
            return false;
        }
        let tokens = route_tokens(set, route);
        tokens.first().copied() == Some(start) && tokens.last().copied() == Some(end)
    })
}

pub(crate) fn edge_kinds(set: &RouteSet, from: Token, to: Token) -> Option<u32> {
    set.edges
        .iter()
        .find(|edge| edge.token_in == from && edge.token_out == to)
        .map(|edge| curve_kinds(&edge.curve))
}

pub(crate) fn topology(set: &RouteSet) -> String {
    let mut signature = String::new();
    for edge in &set.edges {
        signature.push_str(&hex(edge.token_in));
        signature.push('>');
        signature.push_str(&hex(edge.token_out));
        signature.push('|');
    }
    signature.push(';');
    for route in &set.routes {
        for index in &route.edges {
            signature.push_str(&index.to_string());
            signature.push(',');
        }
        signature.push(';');
    }
    signature
}
