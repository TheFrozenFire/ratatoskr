//! How many conduits name each token.

use std::collections::{BTreeMap, HashMap};

use searcher::Token;

use super::Graph;

/// One token and how many stored conduits name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HubScore {
    pub token: Token,
    pub conduits: usize,
}

/// How many tokens meet each connectivity floor, and the highest ranks.
#[derive(Clone, Debug)]
pub(crate) struct HubCensus {
    pub tokens: u64,
    pub at_least: BTreeMap<usize, u64>,
    pub ranked: Vec<HubScore>,
}

const HUB_FLOORS: [usize; 4] = [3, 10, 50, 100];

/// Rank tokens by the number of conduits that name them.
///
/// A conduit in each direction counts twice. `min_conduits` is the floor a
/// token must meet before it can be a hub. `limit` is how many of those tokens
/// the caller wants, highest count first. Equal counts keep the lower token.
pub(crate) fn hub_census(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    min_conduits: usize,
    limit: usize,
) -> HubCensus {
    let mut score: HashMap<Token, usize> = HashMap::new();
    for (token, links) in &graph.out {
        *score.entry(*token).or_default() += links.len();
    }
    for (token, preds) in incoming {
        *score.entry(*token).or_default() += preds.len();
    }
    let mut at_least = BTreeMap::new();
    for floor in HUB_FLOORS {
        at_least.insert(floor, 0);
    }
    for conduits in score.values() {
        for floor in HUB_FLOORS {
            if *conduits >= floor {
                *at_least.entry(floor).or_default() += 1;
            }
        }
    }
    let tokens = score.len() as u64;
    let mut ranked: Vec<HubScore> = score
        .into_iter()
        .filter(|(_, conduits)| *conduits >= min_conduits)
        .map(|(token, conduits)| HubScore { token, conduits })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .conduits
            .cmp(&left.conduits)
            .then(left.token.cmp(&right.token))
    });
    ranked.truncate(limit);
    HubCensus {
        tokens,
        at_least,
        ranked,
    }
}
