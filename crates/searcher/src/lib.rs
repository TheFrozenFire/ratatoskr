//! Route set for one trade.
//!
//! A trade names the tokens it spends, the amount of each, and the tokens it
//! wants back. [`prepare`] checks that trade and records the hubs and the
//! wrapped token. [`execute`] reads a pair conduit for each ordered pair the
//! routes need and returns those curves. The direct pair, a path through one
//! hub, and a path through two hubs are the routes. Native ETH is the zero
//! address. The wrap pays one wrapped token per native token at any size.
//!
//! The caller supplies the conduits. This crate does not open the catalog and
//! does not allocate the flow. A curve is kept only when a route uses it.

use std::collections::{HashMap, HashSet};
use std::fmt;

use num_bigint::BigUint;
use num_traits::Zero;
use routing::SlopeSegment;

/// Native ETH. It has no contract.
pub const NATIVE: Token = Token([0_u8; 20]);

/// One token, named by 20 bytes. Native ETH is [`NATIVE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Token(pub [u8; 20]);

/// Tokens spent, with amounts, and tokens demanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trade {
    pub inputs: Vec<(Token, BigUint)>,
    pub outputs: Vec<Token>,
}

/// A checked trade plus the hubs and the wrapped token the routes may use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub inputs: Vec<(Token, BigUint)>,
    pub outputs: Vec<Token>,
    pub hubs: Vec<Token>,
    pub wrapped: Option<Token>,
}

/// The quote on one ordered pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Curve {
    /// One token in, one token out, at any size. Native ETH and its wrapped token.
    Wrap,
    /// The stored pair conduit.
    Conduit(Vec<SlopeSegment>),
}

/// One curve from a token to a token. Routes share an edge by index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub token_in: Token,
    pub token_out: Token,
    pub curve: Curve,
}

/// Indexes into [`RouteSet::edges`], in order from the input token to an output token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub edges: Vec<usize>,
}

/// What an optimizer allocates. Each curve appears once, and only on a route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSet {
    pub inputs: Vec<(Token, BigUint)>,
    pub outputs: Vec<Token>,
    pub edges: Vec<Edge>,
    pub routes: Vec<Route>,
}

/// The pair conduits a search reads. An empty curve is absent.
pub trait Conduits {
    fn conduit(&self, token_in: Token, token_out: Token) -> Option<Vec<SlopeSegment>>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchError {
    NoInput,
    NoOutput,
    ZeroAmount,
    DuplicateInput,
    DuplicateOutput,
    Overlap,
    DuplicateHub,
    WrappedIsNative,
}

impl fmt::Display for SearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            SearchError::NoInput => "the trade spends nothing",
            SearchError::NoOutput => "the trade demands nothing",
            SearchError::ZeroAmount => "an input amount is zero",
            SearchError::DuplicateInput => "an input token is repeated",
            SearchError::DuplicateOutput => "an output token is repeated",
            SearchError::Overlap => "a token is both spent and demanded",
            SearchError::DuplicateHub => "a hub is repeated",
            SearchError::WrappedIsNative => "the wrapped token is native ETH",
        };
        f.write_str(text)
    }
}

/// Check the trade and keep the hubs and wrapped token the routes may use.
pub fn prepare(trade: Trade, hubs: &[Token], wrapped: Option<Token>) -> Result<Query, SearchError> {
    if trade.inputs.is_empty() {
        return Err(SearchError::NoInput);
    }
    if trade.outputs.is_empty() {
        return Err(SearchError::NoOutput);
    }
    let mut inputs = HashSet::new();
    for (token, amount) in &trade.inputs {
        if amount.is_zero() {
            return Err(SearchError::ZeroAmount);
        }
        if !inputs.insert(*token) {
            return Err(SearchError::DuplicateInput);
        }
    }
    let mut outputs = HashSet::new();
    for token in &trade.outputs {
        if !outputs.insert(*token) {
            return Err(SearchError::DuplicateOutput);
        }
        if inputs.contains(token) {
            return Err(SearchError::Overlap);
        }
    }
    let mut seen_hubs = HashSet::new();
    for hub in hubs {
        if !seen_hubs.insert(*hub) {
            return Err(SearchError::DuplicateHub);
        }
    }
    if wrapped == Some(NATIVE) {
        return Err(SearchError::WrappedIsNative);
    }
    Ok(Query {
        inputs: trade.inputs,
        outputs: trade.outputs,
        hubs: hubs.to_vec(),
        wrapped,
    })
}

/// Read the conduits the query names and return the route set.
pub fn execute(query: &Query, conduits: &impl Conduits) -> RouteSet {
    let mut build = Build {
        query,
        conduits,
        edges: Vec::new(),
        index: HashMap::new(),
        routes: Vec::new(),
        seen: HashSet::new(),
    };
    for (input, _) in &query.inputs {
        for output in &query.outputs {
            build.routes_from(*input, *output);
        }
    }
    let (edges, routes) = retain_used(build.edges, build.routes);
    RouteSet {
        inputs: query.inputs.clone(),
        outputs: query.outputs.clone(),
        edges,
        routes,
    }
}

/// Drop curves that were read while a longer path was still incomplete.
fn retain_used(edges: Vec<Edge>, routes: Vec<Route>) -> (Vec<Edge>, Vec<Route>) {
    let mut used = vec![false; edges.len()];
    for route in &routes {
        for index in &route.edges {
            if let Some(flag) = used.get_mut(*index) {
                *flag = true;
            }
        }
    }
    let mut remap = vec![0; edges.len()];
    let mut kept = Vec::new();
    for (index, edge) in edges.into_iter().enumerate() {
        if used[index] {
            remap[index] = kept.len();
            kept.push(edge);
        }
    }
    let routes = routes
        .into_iter()
        .map(|route| Route {
            edges: route.edges.into_iter().map(|index| remap[index]).collect(),
        })
        .collect();
    (kept, routes)
}

struct Build<'a, C: Conduits> {
    query: &'a Query,
    conduits: &'a C,
    edges: Vec<Edge>,
    index: HashMap<(Token, Token), usize>,
    routes: Vec<Route>,
    seen: HashSet<Vec<usize>>,
}

impl<C: Conduits> Build<'_, C> {
    fn routes_from(&mut self, input: Token, output: Token) {
        for path in self.paths(input, output) {
            self.push(path);
        }
        let Some(wrapped) = self.query.wrapped else {
            return;
        };
        if input == NATIVE {
            if let Some(wrap) = self.intern(NATIVE, wrapped) {
                for path in self.paths(wrapped, output) {
                    let mut edges = vec![wrap];
                    edges.extend(path);
                    self.push(edges);
                }
            }
        }
        if output == NATIVE && input != NATIVE {
            if let Some(wrap) = self.intern(wrapped, NATIVE) {
                if input == wrapped {
                    self.push(vec![wrap]);
                } else {
                    for mut path in self.paths(input, wrapped) {
                        path.push(wrap);
                        self.push(path);
                    }
                }
            }
        }
    }

    fn paths(&mut self, from: Token, to: Token) -> Vec<Vec<usize>> {
        if from == to {
            return Vec::new();
        }
        let mut paths = Vec::new();
        if let Some(direct) = self.intern(from, to) {
            paths.push(vec![direct]);
        }
        for hub in self.query.hubs.clone() {
            if hub == from || hub == to {
                continue;
            }
            let Some(first) = self.intern(from, hub) else {
                continue;
            };
            let Some(second) = self.intern(hub, to) else {
                continue;
            };
            paths.push(vec![first, second]);
        }
        let hubs = self.query.hubs.clone();
        for left in &hubs {
            for right in &hubs {
                if left == right || *left == from || *left == to || *right == from || *right == to {
                    continue;
                }
                let Some(first) = self.intern(from, *left) else {
                    continue;
                };
                let Some(middle) = self.intern(*left, *right) else {
                    continue;
                };
                let Some(last) = self.intern(*right, to) else {
                    continue;
                };
                paths.push(vec![first, middle, last]);
            }
        }
        paths
    }

    fn intern(&mut self, token_in: Token, token_out: Token) -> Option<usize> {
        if token_in == token_out {
            return None;
        }
        if let Some(index) = self.index.get(&(token_in, token_out)) {
            return Some(*index);
        }
        let curve = self.curve(token_in, token_out)?;
        let index = self.edges.len();
        self.edges.push(Edge {
            token_in,
            token_out,
            curve,
        });
        self.index.insert((token_in, token_out), index);
        Some(index)
    }

    fn curve(&self, token_in: Token, token_out: Token) -> Option<Curve> {
        if self.wraps(token_in, token_out) {
            return Some(Curve::Wrap);
        }
        let segments = self.conduits.conduit(token_in, token_out)?;
        if segments.is_empty() {
            return None;
        }
        Some(Curve::Conduit(segments))
    }

    fn wraps(&self, token_in: Token, token_out: Token) -> bool {
        let Some(wrapped) = self.query.wrapped else {
            return false;
        };
        (token_in == NATIVE && token_out == wrapped) || (token_in == wrapped && token_out == NATIVE)
    }

    fn push(&mut self, edges: Vec<usize>) {
        if edges.is_empty() || !self.seen.insert(edges.clone()) {
            return;
        }
        self.routes.push(Route { edges });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_bigint::BigInt;
    use routing::Action;
    use std::collections::HashMap;

    fn token(byte: u8) -> Token {
        Token([byte; 20])
    }

    fn amount(value: u64) -> BigUint {
        BigUint::from(value)
    }

    fn segment(width: i64, output: i64, pool: u8) -> SlopeSegment {
        SlopeSegment {
            width: BigInt::from(width),
            output: BigInt::from(output),
            action: Action {
                kind: 1,
                pool: [pool; 32],
            },
        }
    }

    struct Catalog(HashMap<(Token, Token), Vec<SlopeSegment>>);

    impl Conduits for Catalog {
        fn conduit(&self, token_in: Token, token_out: Token) -> Option<Vec<SlopeSegment>> {
            self.0.get(&(token_in, token_out)).cloned()
        }
    }

    fn trade(inputs: &[(u8, u64)], outputs: &[u8]) -> Trade {
        Trade {
            inputs: inputs
                .iter()
                .map(|(byte, value)| (token(*byte), amount(*value)))
                .collect(),
            outputs: outputs.iter().copied().map(token).collect(),
        }
    }

    #[test]
    fn direct_conduit_is_one_route() {
        let query = prepare(trade(&[(1, 50)], &[2]), &[], None).unwrap();
        let curve = vec![segment(10, 9, 7)];
        let catalog = Catalog(HashMap::from([((token(1), token(2)), curve.clone())]));
        let set = execute(&query, &catalog);
        assert_eq!(set.inputs, vec![(token(1), amount(50))]);
        assert_eq!(set.outputs, vec![token(2)]);
        assert_eq!(set.routes, vec![Route { edges: vec![0] }]);
        assert_eq!(set.edges[0].curve, Curve::Conduit(curve));
    }

    #[test]
    fn one_hub_joins_two_conduits() {
        let hub = token(9);
        let query = prepare(trade(&[(1, 10)], &[2]), &[hub], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), hub), vec![segment(4, 4, 1)]),
            ((hub, token(2)), vec![segment(4, 3, 2)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes, vec![Route { edges: vec![0, 1] }]);
        assert_eq!(set.edges[0].token_out, hub);
        assert_eq!(set.edges[1].token_in, hub);
    }

    #[test]
    fn two_hubs_insert_the_conduit_between_them() {
        let left = token(8);
        let right = token(9);
        let query = prepare(trade(&[(1, 10)], &[2]), &[left, right], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), left), vec![segment(1, 1, 1)]),
            ((left, right), vec![segment(1, 1, 2)]),
            ((right, token(2)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(
            set.routes,
            vec![Route {
                edges: vec![0, 1, 2]
            }]
        );
    }

    #[test]
    fn a_missing_leg_drops_the_hub_route() {
        let hub = token(9);
        let query = prepare(trade(&[(1, 10)], &[2]), &[hub], None).unwrap();
        let catalog = Catalog(HashMap::from([((token(1), hub), vec![segment(1, 1, 1)])]));
        let set = execute(&query, &catalog);
        assert!(set.routes.is_empty());
        assert!(set.edges.is_empty());
    }

    #[test]
    fn an_empty_conduit_is_absent() {
        let query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([((token(1), token(2)), vec![])]));
        let set = execute(&query, &catalog);
        assert!(set.routes.is_empty());
        assert!(set.edges.is_empty());
    }

    #[test]
    fn shared_leg_is_one_edge() {
        let hub = token(9);
        let query = prepare(trade(&[(1, 10)], &[2, 3]), &[hub], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), hub), vec![segment(5, 5, 1)]),
            ((hub, token(2)), vec![segment(5, 4, 2)]),
            ((hub, token(3)), vec![segment(5, 3, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 2);
        assert_eq!(set.routes[0].edges[0], set.routes[1].edges[0]);
        assert_eq!(set.edges.len(), 3);
    }

    #[test]
    fn native_input_wraps_then_uses_the_wrapped_conduits() {
        let wrapped = token(1);
        let out = token(2);
        let query = prepare(
            Trade {
                inputs: vec![(NATIVE, amount(10))],
                outputs: vec![out],
            },
            &[],
            Some(wrapped),
        )
        .unwrap();
        let catalog = Catalog(HashMap::from([((wrapped, out), vec![segment(8, 7, 4)])]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes, vec![Route { edges: vec![0, 1] }]);
        assert_eq!(set.edges[0].curve, Curve::Wrap);
        assert_eq!(set.edges[0].token_in, NATIVE);
        assert_eq!(set.edges[0].token_out, wrapped);
        assert!(matches!(set.edges[1].curve, Curve::Conduit(_)));
    }

    #[test]
    fn wrapped_input_unwraps_to_native() {
        let wrapped = token(1);
        let query = prepare(
            Trade {
                inputs: vec![(wrapped, amount(4))],
                outputs: vec![NATIVE],
            },
            &[],
            Some(wrapped),
        )
        .unwrap();
        let set = execute(&query, &Catalog(HashMap::new()));
        assert_eq!(set.routes, vec![Route { edges: vec![0] }]);
        assert_eq!(set.edges[0].curve, Curve::Wrap);
        assert_eq!(set.edges[0].token_out, NATIVE);
    }

    #[test]
    fn a_leg_that_no_route_uses_is_dropped() {
        let dropped = token(8);
        let kept = token(9);
        let query = prepare(trade(&[(1, 10)], &[2]), &[dropped, kept], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), dropped), vec![segment(1, 1, 1)]),
            ((token(1), kept), vec![segment(1, 1, 2)]),
            ((kept, token(2)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes, vec![Route { edges: vec![0, 1] }]);
        assert_eq!(set.edges.len(), 2);
        assert!(set
            .edges
            .iter()
            .all(|edge| edge.token_in != dropped && edge.token_out != dropped));
    }

    #[test]
    fn native_with_no_onward_conduit_keeps_no_edge() {
        let wrapped = token(1);
        let query = prepare(
            Trade {
                inputs: vec![(NATIVE, amount(1))],
                outputs: vec![token(2)],
            },
            &[],
            Some(wrapped),
        )
        .unwrap();
        let set = execute(&query, &Catalog(HashMap::new()));
        assert!(set.routes.is_empty());
        assert!(set.edges.is_empty());
    }

    #[test]
    fn prepare_rejects_a_token_on_both_sides() {
        let err = prepare(trade(&[(1, 10)], &[1]), &[], None).unwrap_err();
        assert_eq!(err, SearchError::Overlap);
    }

    #[test]
    fn prepare_rejects_a_zero_amount() {
        let err = prepare(trade(&[(1, 0)], &[2]), &[], None).unwrap_err();
        assert_eq!(err, SearchError::ZeroAmount);
    }
}
