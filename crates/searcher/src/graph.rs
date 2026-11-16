//! Directed conduit graph.
//!
//! An edge is one stored pair conduit. Liquidity is the sum of its piece
//! widths. Hubs are the tokens named by the most conduits. This crate does
//! not open the catalog. The caller pushes each conduit it has read.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::Token;

/// One outgoing pair conduit.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub to: Token,
    pub capacity: u128,
}

/// Kinds and liquidity on one ordered pair.
#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub kinds: u32,
    pub capacity: u128,
}

/// Pair conduits indexed by the token they spend.
#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub out: HashMap<Token, Vec<Link>>,
    index: HashMap<(Token, Token), Meta>,
}

impl Graph {
    pub fn push(&mut self, from: Token, to: Token, kinds: u32, capacity: u128) {
        self.index.insert((from, to), Meta { kinds, capacity });
        self.out
            .entry(from)
            .or_default()
            .push(Link { to, capacity });
    }

    pub fn degree(&self, token: Token) -> usize {
        self.out.get(&token).map(|links| links.len()).unwrap_or(0)
    }

    pub fn capacity(&self, token: Token) -> u128 {
        self.out
            .get(&token)
            .map(|links| {
                links
                    .iter()
                    .fold(0_u128, |sum, link| sum.saturating_add(link.capacity))
            })
            .unwrap_or(0)
    }

    pub fn meta(&self, from: Token, to: Token) -> Option<Meta> {
        self.index.get(&(from, to)).copied()
    }

    pub fn neighbors(&self, token: Token) -> HashSet<Token> {
        self.out
            .get(&token)
            .map(|links| links.iter().map(|link| link.to).collect())
            .unwrap_or_default()
    }

    /// Distinct tokens this token pays, in token order.
    pub fn targets(&self, token: Token) -> Vec<Token> {
        let mut tokens = self
            .out
            .get(&token)
            .map(|links| links.iter().map(|link| link.to).collect::<Vec<_>>())
            .unwrap_or_default();
        tokens.sort();
        tokens.dedup();
        tokens
    }
}

/// Who has a conduit into this token, in token order.
pub fn incoming(graph: &Graph) -> HashMap<Token, Vec<Token>> {
    let mut incoming: HashMap<Token, Vec<Token>> = HashMap::new();
    let mut sources: Vec<Token> = graph.out.keys().copied().collect();
    sources.sort();
    for from in sources {
        for next in graph.targets(from) {
            incoming.entry(next).or_default().push(from);
        }
    }
    for preds in incoming.values_mut() {
        preds.sort();
        preds.dedup();
    }
    incoming
}

/// One token and how many stored conduits name it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HubScore {
    pub token: Token,
    pub conduits: usize,
}

/// How many tokens meet each connectivity floor, and the highest ranks.
#[derive(Clone, Debug)]
pub struct HubCensus {
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
pub fn hub_census(
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

#[cfg(test)]
mod tests {
    use super::*;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    #[test]
    fn hubs_follow_the_number_of_conduits() {
        let mut graph = Graph::default();
        graph.push(token(1), token(9), 1, 10);
        graph.push(token(2), token(9), 1, 10);
        graph.push(token(9), token(3), 1, 10);
        graph.push(token(8), token(4), 1, 10);
        let incoming = incoming(&graph);
        let census = hub_census(&graph, &incoming, 3, 8);
        assert_eq!(census.tokens, 6);
        assert_eq!(census.ranked.len(), 1);
        assert_eq!(census.ranked[0].token, token(9));
        assert_eq!(census.ranked[0].conduits, 3);
        assert_eq!(census.at_least.get(&3).copied(), Some(1));
        assert_eq!(census.at_least.get(&10).copied(), Some(0));
    }
}
