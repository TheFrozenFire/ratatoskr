//! Directed conduit graph used to choose trades.
//!
//! An edge is one stored pair conduit. Liquidity is the sum of its piece
//! widths. Distance counts conduits. The first hub in the list wins when two
//! hubs are the same distance away.

use std::collections::{HashMap, HashSet};

use searcher::Token;

mod census;
mod reach;
mod select;

pub(crate) use census::{hub_census, HubCensus};
pub(crate) use reach::{incoming, reach_of, sole_hub, HubReach};
pub(crate) use select::{select, Deep, PathEnds, Selection};

#[derive(Clone, Copy, Debug)]
pub(crate) struct Link {
    pub to: Token,
    pub capacity: u128,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Meta {
    pub kinds: u32,
    pub capacity: u128,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Graph {
    pub out: HashMap<Token, Vec<Link>>,
    index: HashMap<(Token, Token), Meta>,
}

impl Graph {
    pub(crate) fn push(&mut self, from: Token, to: Token, kinds: u32, capacity: u128) {
        self.index.insert((from, to), Meta { kinds, capacity });
        self.out
            .entry(from)
            .or_default()
            .push(Link { to, capacity });
    }

    pub(crate) fn degree(&self, token: Token) -> usize {
        self.out.get(&token).map(|links| links.len()).unwrap_or(0)
    }

    pub(crate) fn capacity(&self, token: Token) -> u128 {
        self.out
            .get(&token)
            .map(|links| {
                links
                    .iter()
                    .fold(0_u128, |sum, link| sum.saturating_add(link.capacity))
            })
            .unwrap_or(0)
    }

    pub(crate) fn meta(&self, from: Token, to: Token) -> Option<Meta> {
        self.index.get(&(from, to)).copied()
    }

    pub(crate) fn neighbors(&self, token: Token) -> HashSet<Token> {
        self.out
            .get(&token)
            .map(|links| links.iter().map(|link| link.to).collect())
            .unwrap_or_default()
    }
}

fn targets(graph: &Graph, token: Token) -> Vec<Token> {
    let mut tokens = graph
        .out
        .get(&token)
        .map(|links| links.iter().map(|link| link.to).collect::<Vec<_>>())
        .unwrap_or_default();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn rebuild(parent: &HashMap<Token, Token>, from: Token, to: Token) -> Vec<Token> {
    let mut path = vec![to];
    let mut cursor = to;
    while cursor != from {
        cursor = parent[&cursor];
        path.push(cursor);
    }
    path.reverse();
    path
}

fn native(token: Token) -> bool {
    token.0 == [0_u8; 20]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn link(graph: &mut Graph, from: u8, to: u8) {
        graph.push(token(from), token(to), 1, 10);
    }

    #[test]
    fn a_tendril_on_each_side_of_a_hub_is_a_long_route() {
        let mut graph = Graph::default();
        let forward = [1_u8, 2, 3, 9, 4, 5, 6];
        for pair in forward.windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
        let selection = select(&graph, &[token(9)]);
        let longest = selection
            .long
            .iter()
            .max_by_key(|path| path.path.len())
            .expect("long path");
        assert_eq!(longest.path.first().copied(), Some(token(1)));
        assert_eq!(longest.path.last().copied(), Some(token(6)));
        assert_eq!(longest.path.len(), 7);
        assert!(selection
            .via_hub
            .iter()
            .any(|path| path.path == vec![token(3), token(9), token(4)]));
        assert!(selection
            .deep_to
            .iter()
            .any(|deep| deep.token == token(1) && deep.dist == 3));
        assert!(selection
            .deep_from
            .iter()
            .any(|deep| deep.token == token(6) && deep.dist == 3));
    }

    #[test]
    fn direct_neighbors_are_not_a_long_route() {
        let mut graph = Graph::default();
        link(&mut graph, 1, 9);
        link(&mut graph, 9, 2);
        link(&mut graph, 1, 2);
        let selection = select(&graph, &[token(9)]);
        assert!(selection.long.is_empty());
        assert!(selection.via_hub.is_empty());
    }

    #[test]
    fn a_shared_counterparty_is_not_a_long_end() {
        let mut graph = Graph::default();
        for pair in [1_u8, 2, 9, 3, 4].windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
        link(&mut graph, 1, 8);
        link(&mut graph, 4, 8);
        let selection = select(&graph, &[token(9)]);
        assert!(selection.long.is_empty());
    }

    #[test]
    fn a_three_conduit_shortcut_is_not_long() {
        let mut graph = Graph::default();
        for pair in [1_u8, 2, 9, 3, 4].windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
        for pair in [1_u8, 5, 6, 4].windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
        let selection = select(&graph, &[token(9)]);
        assert!(selection.long.is_empty());
    }

    #[test]
    fn a_shorter_path_of_four_conduits_is_the_catalog_path() {
        let mut graph = Graph::default();
        for pair in [1_u8, 2, 3, 9, 4, 5, 6].windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
        link(&mut graph, 3, 5);
        let selection = select(&graph, &[token(9)]);
        let path = selection
            .long
            .iter()
            .find(|path| {
                path.path.first() == Some(&token(1)) && path.path.last() == Some(&token(6))
            })
            .expect("path");
        assert_eq!(
            path.path,
            vec![token(1), token(2), token(3), token(5), token(6)]
        );
    }

    #[test]
    fn tendrils_around_two_hubs_cross_on_the_hub_conduit() {
        let mut graph = Graph::default();
        for pair in [1_u8, 2, 8].windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
        link(&mut graph, 8, 9);
        for pair in [9_u8, 3, 4].windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
        let selection = select(&graph, &[token(8), token(9)]);
        let path = selection
            .long
            .iter()
            .find(|path| path.cross_hub)
            .expect("cross");
        assert_eq!(
            path.path,
            vec![token(1), token(2), token(8), token(9), token(3), token(4)]
        );
    }

    #[test]
    fn one_hub_between_two_leaves_is_a_via_path() {
        let mut graph = Graph::default();
        link(&mut graph, 1, 9);
        link(&mut graph, 9, 2);
        let selection = select(&graph, &[token(9)]);
        assert!(selection.long.is_empty());
        assert_eq!(selection.via_hub.len(), 1);
        assert_eq!(
            selection.via_hub[0].path,
            vec![token(1), token(9), token(2)]
        );
    }

    #[test]
    fn a_component_away_from_the_hubs_is_isolated() {
        let mut graph = Graph::default();
        for pair in [1_u8, 2, 3, 4, 5].windows(2) {
            link(&mut graph, pair[0], pair[1]);
        }
        let selection = select(&graph, &[token(9)]);
        assert!(selection.outside >= 5);
        assert!(selection.outside_span >= 4);
        assert!(selection
            .isolated
            .iter()
            .any(|path| path.path.len() >= 5 && path.path.first() == Some(&token(1))));
    }

    #[test]
    fn hubs_follow_the_number_of_conduits() {
        let mut graph = Graph::default();
        link(&mut graph, 1, 9);
        link(&mut graph, 2, 9);
        link(&mut graph, 9, 3);
        link(&mut graph, 8, 4);
        let incoming = incoming(&graph);
        let census = hub_census(&graph, &incoming, 3, 8);
        assert_eq!(census.tokens, 6);
        assert_eq!(census.ranked.len(), 1);
        assert_eq!(census.ranked[0].token, token(9));
        assert_eq!(census.ranked[0].conduits, 3);
        assert_eq!(census.at_least.get(&3).copied(), Some(1));
        assert_eq!(census.at_least.get(&10).copied(), Some(0));
        let reach = reach_of(&graph, &incoming, &[token(9)]);
        assert_eq!(reach.toward, 2);
        assert_eq!(reach.from_hub, 1);
        assert_eq!(reach.outside, 2);
        assert_eq!(reach.both, 0);
    }
}
