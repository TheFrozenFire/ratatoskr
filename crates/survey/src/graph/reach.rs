//! Directed reach into and out of the hub set.

use std::collections::{HashMap, HashSet, VecDeque};

use searcher::{Graph, Token, NATIVE};

/// Tokens with a directed path into the hub set, out of it, both, or neither.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HubReach {
    pub both: u64,
    pub toward: u64,
    pub from_hub: u64,
    pub outside: u64,
}

/// Directed reach of this hub list. Native ETH is omitted from the counts.
pub(crate) fn reach_of(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &[Token],
) -> HubReach {
    let hub_set: HashSet<Token> = hubs.iter().copied().collect();
    let outward = forward(graph);
    let toward = walk(hubs, incoming);
    let away = walk(hubs, &outward);
    let mut reach = HubReach::default();
    let mut all = HashSet::new();
    for token in graph.out.keys() {
        all.insert(*token);
    }
    for token in incoming.keys() {
        all.insert(*token);
    }
    for token in all {
        if hub_set.contains(&token) || token == NATIVE {
            continue;
        }
        match (
            toward.dist.contains_key(&token),
            away.dist.contains_key(&token),
        ) {
            (true, true) => reach.both += 1,
            (true, false) => reach.toward += 1,
            (false, true) => reach.from_hub += 1,
            (false, false) => reach.outside += 1,
        }
    }
    reach
}

/// The one hub among this token's counterparties, when there is exactly one.
pub(crate) fn sole_hub(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    token: Token,
    hubs: &HashSet<Token>,
) -> Option<Token> {
    let mut found = None;
    let mut consider = |other: Token| -> bool {
        if !hubs.contains(&other) {
            return true;
        }
        match found {
            None => {
                found = Some(other);
                true
            }
            Some(hub) => hub == other,
        }
    };
    for next in graph.neighbors(token) {
        if !consider(next) {
            return None;
        }
    }
    if let Some(preds) = incoming.get(&token) {
        for pred in preds {
            if !consider(*pred) {
                return None;
            }
        }
    }
    found
}

pub(super) struct Walk {
    pub(super) dist: HashMap<Token, u32>,
    pub(super) prev: HashMap<Token, Token>,
    pub(super) origin: HashMap<Token, Token>,
}

pub(super) fn forward(graph: &Graph) -> HashMap<Token, Vec<Token>> {
    let mut out = HashMap::new();
    let mut sources: Vec<Token> = graph.out.keys().copied().collect();
    sources.sort();
    for token in sources {
        out.insert(token, graph.targets(token));
    }
    out
}

pub(super) fn walk(roots: &[Token], next: &HashMap<Token, Vec<Token>>) -> Walk {
    let mut walk = Walk {
        dist: HashMap::new(),
        prev: HashMap::new(),
        origin: HashMap::new(),
    };
    let mut queue = VecDeque::new();
    for root in roots {
        if walk.dist.insert(*root, 0).is_none() {
            walk.origin.insert(*root, *root);
            queue.push_back(*root);
        }
    }
    while let Some(node) = queue.pop_front() {
        let depth = walk.dist[&node];
        let origin = walk.origin[&node];
        let Some(children) = next.get(&node) else {
            continue;
        };
        for child in children {
            if walk.dist.contains_key(child) {
                continue;
            }
            walk.dist.insert(*child, depth + 1);
            walk.prev.insert(*child, node);
            walk.origin.insert(*child, origin);
            queue.push_back(*child);
        }
    }
    walk
}
