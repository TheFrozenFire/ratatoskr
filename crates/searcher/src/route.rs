//! The direct pair, a path through one hub, a path through two hubs, and the wrap.

use std::collections::{HashMap, HashSet};

use action::{Token, NATIVE};
use num_bigint::BigUint;
use routing::SlopeSegment;

use crate::query::Query;

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

/// The curves the search returns. Each curve appears once, and only on a route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteSet {
    pub inputs: Vec<(Token, BigUint)>,
    pub outputs: Vec<Token>,
    pub edges: Vec<Edge>,
    pub routes: Vec<Route>,
}

/// The pair conduits a search reads. An empty curve is absent.
///
/// [`Conduits::outgoing`] and [`Conduits::incoming`] list the other side of
/// those conduits when the token names at most `limit` of them. `None` means
/// the list is wider than `limit`, or the caller cannot list it. The walk then
/// probes that token for a conduit to the other endpoint and for a conduit to
/// each hub, and does not expand it.
pub trait Conduits {
    fn conduit(&self, token_in: Token, token_out: Token) -> Option<Vec<SlopeSegment>>;

    fn outgoing(&self, _token: Token, _limit: usize) -> Option<Vec<Token>> {
        None
    }

    fn incoming(&self, _token: Token, _limit: usize) -> Option<Vec<Token>> {
        None
    }
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

pub(crate) struct Build<'a, C: Conduits> {
    pub(crate) query: &'a Query,
    pub(crate) conduits: &'a C,
    pub(crate) edges: Vec<Edge>,
    pub(crate) index: HashMap<(Token, Token), usize>,
    pub(crate) routes: Vec<Route>,
    pub(crate) seen: HashSet<Vec<usize>>,
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
        paths.extend(self.walk_routes(from, to));
        paths
    }
    pub(crate) fn intern(&mut self, token_in: Token, token_out: Token) -> Option<usize> {
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
    pub(crate) fn curve(&self, token_in: Token, token_out: Token) -> Option<Curve> {
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
