use std::collections::{BTreeMap, HashMap, HashSet};

use searcher::{Conduits, Curve, Route, RouteSet, Token, NEIGHBOR_LIMIT, WALK_DEPTH};
use venues::kind_name;

use crate::graph::{Graph, HubCensus, HubReach, Selection};
use crate::kinds::{curve_kinds, curve_label, kind_list};
use crate::reader::Reader;
use crate::scan::Survey;
use crate::scenario::Scenario;
use crate::source::Source;
use crate::token::{label, Known};
use crate::{HUB_LIMITS, HUB_MINIMUM, QUERY_HUBS};

pub(crate) fn push_header(
    out: &mut String,
    survey: &Survey,
    known: &[Known],
    hubs: &[Token],
    selection: &Selection,
    census: &HubCensus,
    reach_rows: &[(usize, HubReach)],
) {
    out.push_str(&format!(
        "conduits {}\ntokens with an outgoing curve {}\ntokens named by a conduit {}\nhub minimum {HUB_MINIMUM}\nquery hubs {QUERY_HUBS}\nneighbor limit {NEIGHBOR_LIMIT}\nwalk depth {WALK_DEPTH}\n",
        survey.conduits,
        survey.graph.out.len(),
        census.tokens
    ));
    for (floor, count) in &census.at_least {
        out.push_str(&format!("tokens with at least {floor} conduits: {count}\n"));
    }
    for limit in HUB_LIMITS {
        let Some(hub) = census.ranked.get(limit - 1) else {
            continue;
        };
        out.push_str(&format!(
            "hub rank {limit} {} conduits {}\n",
            label(hub.token, known),
            hub.conduits
        ));
    }
    out.push_str("hubs");
    for hub in hubs {
        let conduits = census
            .ranked
            .iter()
            .find(|score| score.token == *hub)
            .map(|score| score.conduits)
            .unwrap_or(0);
        out.push_str(&format!(
            " {} conduits={conduits} out={}",
            label(*hub, known),
            survey.graph.degree(*hub)
        ));
    }
    out.push('\n');
    for (limit, reach) in reach_rows {
        out.push_str(&format!(
            "reach K={limit} both {} toward {} from {} outside {}\n",
            reach.both, reach.toward, reach.from_hub, reach.outside
        ));
    }
    let mut pure: Vec<u8> = survey.pure.keys().copied().collect();
    pure.sort_unstable();
    out.push_str("pure kinds");
    for kind in pure {
        out.push(' ');
        out.push_str(kind_name(kind));
    }
    out.push('\n');
    let mut widths: Vec<u32> = survey.by_width.keys().copied().collect();
    widths.sort_unstable();
    out.push_str("mix widths");
    for width in widths {
        out.push_str(&format!(" {width}"));
    }
    out.push('\n');
    out.push_str(&format!(
        "reach both {} toward {} from {} outside {}\n",
        selection.reached_both,
        selection.only_toward_hub,
        selection.only_from_hub,
        selection.outside
    ));
    out.push_str(&format!(
        "outside component edges {} longest directed distance {}\n",
        selection.outside_edges, selection.outside_span
    ));
    out.push_str(&format!(
        "farthest token toward a hub {} (small {})\nfarthest token from a hub {} (small {})\n",
        selection.max_to_hub,
        selection.max_to_hub_small,
        selection.max_from_hub,
        selection.max_from_hub_small
    ));
    out.push_str(&format!(
        "small toward hub {}\nsmall from hub {}\n",
        hist_line(&selection.to_hist),
        hist_line(&selection.from_hist)
    ));
    out.push_str(&format!(
        "kinds on small tendrils of 4 or more conduits toward a hub: {}\n",
        kind_list(selection.far_to_kinds)
    ));
    out.push_str(&format!(
        "kinds on small tendrils of 4 or more conduits from a hub: {}\n\n",
        kind_list(selection.far_from_kinds)
    ));
}

pub(crate) fn hist_line(hist: &BTreeMap<u32, u64>) -> String {
    if hist.is_empty() {
        return "-".into();
    }
    hist.iter()
        .map(|(dist, count)| format!("{dist}:{count}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn push_set<S: Source>(
    out: &mut String,
    scenario: &Scenario,
    set: &RouteSet,
    known: &[Known],
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    reader: &Reader<'_, S>,
) {
    out.push_str(&format!("== {}\n{}\n", scenario.name, scenario.detail));
    for (token, value) in &scenario.trade.inputs {
        out.push_str(&format!("in {} {}\n", label(*token, known), value));
    }
    out.push_str("out");
    for (token, _) in &scenario.trade.outputs {
        out.push(' ');
        out.push_str(&label(*token, known));
    }
    out.push('\n');
    if scenario.show_path {
        for (nth, path) in scenario.paths.iter().enumerate() {
            out.push_str(&format!("catalog path {nth}\n"));
            if let Some(first) = path.first() {
                out.push_str(&format!("  {}\n", end_line(graph, incoming, *first, known)));
            }
            if let Some(last) = path.last() {
                out.push_str(&format!("  {}\n", end_line(graph, incoming, *last, known)));
            }
            for (index, pair) in path.windows(2).enumerate() {
                out.push_str(&format!(
                    "  {index} {}\n",
                    hop_line(graph, reader, pair[0], pair[1], known)
                ));
            }
        }
    }
    let shapes = shapes(set);
    out.push_str(&format!(
        "routes {} edges {}\nshapes direct {} one {} two {} longer {} wrap {}\n",
        set.routes.len(),
        set.edges.len(),
        shapes.direct,
        shapes.one,
        shapes.two,
        shapes.longer,
        shapes.wrap
    ));
    let mut kinds = 0_u32;
    for (index, edge) in set.edges.iter().enumerate() {
        let edge_kinds = curve_kinds(&edge.curve);
        kinds |= edge_kinds;
        out.push_str(&format!(
            "  edge {index} {} -> {} {}\n",
            label(edge.token_in, known),
            label(edge.token_out, known),
            curve_label(&edge.curve)
        ));
    }
    for (index, route) in set.routes.iter().enumerate() {
        out.push_str(&format!(
            "  route {index} {}\n",
            route_line(set, route, known)
        ));
    }
    out.push_str(&format!("kinds {}\n\n", kind_list(kinds)));
}

pub(crate) fn end_line(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    token: Token,
    known: &[Known],
) -> String {
    let indeg = incoming.get(&token).map(|preds| preds.len()).unwrap_or(0);
    format!(
        "{} out {} in {} width {}",
        label(token, known),
        graph.degree(token),
        indeg,
        graph.capacity(token)
    )
}

pub(crate) fn hop_line<S: Source>(
    graph: &Graph,
    reader: &Reader<'_, S>,
    from: Token,
    to: Token,
    known: &[Known],
) -> String {
    let meta = graph.meta(from, to);
    let loaded = reader.conduit(from, to);
    let segments = loaded.as_ref().map(|items| items.len()).unwrap_or(0);
    let pools = loaded
        .as_ref()
        .map(|items| {
            items
                .iter()
                .map(|segment| segment.action.pool)
                .collect::<HashSet<_>>()
                .len()
        })
        .unwrap_or(0);
    let (kinds, capacity) = meta
        .map(|meta| (kind_list(meta.kinds), meta.capacity.to_string()))
        .unwrap_or_else(|| ("-".into(), "-".into()));
    format!(
        "{} -> {} width {capacity} kinds {kinds} segments {segments} pools {pools}",
        label(from, known),
        label(to, known)
    )
}

pub(crate) struct Shapes {
    pub(crate) direct: usize,
    pub(crate) one: usize,
    pub(crate) two: usize,
    pub(crate) longer: usize,
    pub(crate) wrap: usize,
}

pub(crate) fn shapes(set: &RouteSet) -> Shapes {
    let mut shapes = Shapes {
        direct: 0,
        one: 0,
        two: 0,
        longer: 0,
        wrap: 0,
    };
    for route in &set.routes {
        let mut content = 0_usize;
        let mut wrap = false;
        for index in &route.edges {
            if matches!(set.edges[*index].curve, Curve::Wrap) {
                wrap = true;
            } else {
                content += 1;
            }
        }
        if wrap {
            shapes.wrap += 1;
        }
        match content {
            1 => shapes.direct += 1,
            2 => shapes.one += 1,
            3 => shapes.two += 1,
            n if n > 3 => shapes.longer += 1,
            _ => {}
        }
    }
    shapes
}

pub(crate) fn route_line(set: &RouteSet, route: &Route, known: &[Known]) -> String {
    let Some(first) = route.edges.first() else {
        return "-".into();
    };
    let mut parts = vec![label(set.edges[*first].token_in, known)];
    for index in &route.edges {
        parts.push(label(set.edges[*index].token_out, known));
    }
    parts.join(" -> ")
}

pub(crate) fn note_hubs(census: &HubCensus, known: &[Known]) {
    eprint!("ranked");
    for hub in census.ranked.iter().take(QUERY_HUBS) {
        eprint!(" {}={}", label(hub.token, known), hub.conduits);
    }
    eprintln!();
    for (floor, count) in &census.at_least {
        eprintln!("tokens with at least {floor} conduits: {count}");
    }
}

pub(crate) fn note_degrees(graph: &Graph, known: &[Known]) {
    let mut ranks: Vec<(usize, Token)> = graph
        .out
        .iter()
        .map(|(token, links)| (links.len(), *token))
        .collect();
    ranks.sort_by(|left, right| right.0.cmp(&left.0).then(left.1.cmp(&right.1)));
    ranks.truncate(12);
    eprint!("widest");
    for (degree, token) in ranks {
        eprint!(" {}={degree}", label(token, known));
    }
    eprintln!();
}
