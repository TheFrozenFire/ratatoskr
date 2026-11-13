//! Route set for one trade.
//!
//! A trade names the tokens it spends, the amount of each, and the tokens it
//! wants back. [`prepare`] checks that trade and records the hubs, the wrapped
//! token, and the bounds of the endpoint walk. [`execute`] reads a pair conduit
//! for each ordered pair the routes need and returns those curves. The routes
//! are the direct pair, a path through one hub, a path through two hubs, and a
//! bounded walk from an endpoint that names few conduits. That walk keeps a
//! hub-free path, and it joins a tendril into a hub with a tendril leaving a hub.
//! A wider token is not expanded. The walk asks whether that token has one
//! conduit to the other endpoint, and whether it has one conduit to a hub.
//! Native ETH is the zero address. The wrap pays one wrapped token per native
//! token at any size.
//!
//! The caller supplies the conduits. This crate does not open the catalog and
//! does not allocate the flow. A curve is kept only when a route uses it.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use num_bigint::BigUint;
use num_traits::Zero;
use routing::SlopeSegment;

pub use action::{Token, NATIVE};

/// The walk lists a token's conduits only when there are at most this many.
pub const NEIGHBOR_LIMIT: usize = 64;

/// Most conduits on one walked path.
pub const WALK_DEPTH: usize = 8;

/// Nodes the walk may enqueue from one endpoint.
const WALK_NODES: usize = 16_384;

/// Direct probes from a token the walk refuses to expand.
const FAT_PROBES: usize = 64;

/// Tokens spent, with amounts, and tokens demanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trade {
    pub inputs: Vec<(Token, BigUint)>,
    pub outputs: Vec<Token>,
}

/// A checked trade plus the hubs, the wrapped token, and the walk bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub inputs: Vec<(Token, BigUint)>,
    pub outputs: Vec<Token>,
    pub hubs: Vec<Token>,
    pub wrapped: Option<Token>,
    /// List a token's conduits only when there are at most this many.
    pub neighbor_limit: usize,
    /// Most conduits in one walked path.
    pub walk_depth: usize,
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

impl std::error::Error for SearchError {}

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
        neighbor_limit: NEIGHBOR_LIMIT,
        walk_depth: WALK_DEPTH,
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
        paths.extend(self.walk_routes(from, to));
        paths
    }

    fn walk_routes(&mut self, from: Token, to: Token) -> Vec<Vec<usize>> {
        if self.query.walk_depth == 0 || self.query.neighbor_limit == 0 {
            return Vec::new();
        }
        let hubs: HashSet<Token> = self.query.hubs.iter().copied().collect();
        let forward = self.side(from, to, &hubs, true);
        let backward = self.side(to, from, &hubs, false);
        let mut paths = Vec::new();
        for goal in &forward.goals {
            if let Some(edges) = self.concretize(goal) {
                paths.push(edges);
            }
        }
        for goal in &backward.goals {
            let mut tokens = goal.clone();
            tokens.reverse();
            if let Some(edges) = self.concretize(&tokens) {
                paths.push(edges);
            }
        }
        let mut into_hubs: Vec<Token> = forward.hubs.keys().copied().collect();
        let mut out_of_hubs: Vec<Token> = backward.hubs.keys().copied().collect();
        into_hubs.sort();
        out_of_hubs.sort();
        for hub_in in &into_hubs {
            let Some(inbound) = forward.hubs.get(hub_in) else {
                continue;
            };
            for hub_out in &out_of_hubs {
                let Some(outbound) = backward.hubs.get(hub_out) else {
                    continue;
                };
                let mut leaving = outbound.clone();
                leaving.reverse();
                if let Some(edges) = self.join_tokens(inbound, &leaving) {
                    paths.push(edges);
                }
            }
        }
        paths
    }

    fn side(&self, start: Token, goal: Token, hubs: &HashSet<Token>, forward: bool) -> Found {
        let mut found = Found::default();
        if start == goal {
            return found;
        }
        if hubs.contains(&start) {
            found.hubs.insert(start, vec![start]);
            return found;
        }
        if self.listed(start, forward).is_none() {
            return found;
        }
        let mut parent = HashMap::new();
        let mut depth = HashMap::new();
        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        depth.insert(start, 0_usize);
        seen.insert(start);
        queue.push_back(start);
        let mut nodes = 1_usize;
        let mut probes = 0_usize;
        let mut bridges = Vec::new();
        let mut longer: Option<Vec<Token>> = None;
        while let Some(node) = queue.pop_front() {
            let node_depth = depth[&node];
            if node_depth >= self.query.walk_depth {
                continue;
            }
            let Some(neighbors) = self.listed(node, forward) else {
                continue;
            };
            for next in neighbors {
                if next == node || next == start {
                    continue;
                }
                if hubs.contains(&next) {
                    let mut path = token_path(&parent, start, node);
                    path.push(next);
                    remember_hub(&mut found.hubs, next, path);
                    continue;
                }
                if next == goal {
                    remember_goal(
                        &mut bridges,
                        &mut longer,
                        &token_path(&parent, start, node),
                        goal,
                    );
                    continue;
                }
                if seen.contains(&next) {
                    continue;
                }
                if self.listed(next, forward).is_none() {
                    if probes < FAT_PROBES && node_depth + 2 <= self.query.walk_depth {
                        probes += 1;
                        self.probe_wide(&mut Probe {
                            found: &mut found,
                            hubs,
                            parent: &parent,
                            bridges: &mut bridges,
                            longer: &mut longer,
                            start,
                            node,
                            next,
                            goal,
                            forward,
                        });
                    }
                    continue;
                }
                if nodes >= WALK_NODES || node_depth + 1 >= self.query.walk_depth {
                    continue;
                }
                seen.insert(next);
                parent.insert(next, node);
                depth.insert(next, node_depth + 1);
                nodes += 1;
                queue.push_back(next);
            }
        }
        found.goals = bridges;
        if let Some(path) = longer {
            found.goals.push(path);
        }
        found
    }

    fn probe_wide(&self, probe: &mut Probe<'_>) {
        let mut via = token_path(probe.parent, probe.start, probe.node);
        via.push(probe.next);
        let (src, dst) = if probe.forward {
            (probe.next, probe.goal)
        } else {
            (probe.goal, probe.next)
        };
        if self.curve(src, dst).is_some() {
            remember_goal(probe.bridges, probe.longer, &via, probe.goal);
        }
        let mut ordered: Vec<Token> = probe.hubs.iter().copied().collect();
        ordered.sort();
        for hub in ordered {
            if via.contains(&hub) {
                continue;
            }
            let prospect = via.len() + 1;
            if probe
                .found
                .hubs
                .get(&hub)
                .is_some_and(|kept| kept.len() <= prospect)
            {
                continue;
            }
            let reaches = if probe.forward {
                self.curve(probe.next, hub).is_some()
            } else {
                self.curve(hub, probe.next).is_some()
            };
            if !reaches {
                continue;
            }
            let mut tendril = via.clone();
            tendril.push(hub);
            if distinct(&tendril) {
                remember_hub(&mut probe.found.hubs, hub, tendril);
            }
        }
    }

    fn listed(&self, token: Token, forward: bool) -> Option<Vec<Token>> {
        let limit = self.query.neighbor_limit;
        let mut tokens = if forward {
            self.conduits.outgoing(token, limit)?
        } else {
            self.conduits.incoming(token, limit)?
        };
        if tokens.len() > limit {
            return None;
        }
        tokens.sort();
        tokens.dedup();
        if tokens.len() > limit {
            None
        } else {
            Some(tokens)
        }
    }

    fn join_tokens(&mut self, inbound: &[Token], outbound: &[Token]) -> Option<Vec<usize>> {
        if inbound.is_empty() || outbound.is_empty() {
            return None;
        }
        let mut tokens = Vec::with_capacity(inbound.len() + outbound.len());
        tokens.extend_from_slice(inbound);
        let rest = if tokens.last() == outbound.first() {
            &outbound[1..]
        } else {
            outbound
        };
        tokens.extend_from_slice(rest);
        if !distinct(&tokens) {
            return None;
        }
        self.concretize(&tokens)
    }

    fn concretize(&mut self, tokens: &[Token]) -> Option<Vec<usize>> {
        if tokens.len() < 2 {
            return None;
        }
        let mut edges = Vec::with_capacity(tokens.len() - 1);
        for pair in tokens.windows(2) {
            edges.push(self.intern(pair[0], pair[1])?);
        }
        Some(edges)
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

#[derive(Default)]
struct Found {
    hubs: HashMap<Token, Vec<Token>>,
    goals: Vec<Vec<Token>>,
}

struct Probe<'a> {
    found: &'a mut Found,
    hubs: &'a HashSet<Token>,
    parent: &'a HashMap<Token, Token>,
    bridges: &'a mut Vec<Vec<Token>>,
    longer: &'a mut Option<Vec<Token>>,
    start: Token,
    node: Token,
    next: Token,
    goal: Token,
    forward: bool,
}

fn remember_hub(hubs: &mut HashMap<Token, Vec<Token>>, hub: Token, path: Vec<Token>) {
    match hubs.entry(hub) {
        Entry::Occupied(mut slot) => {
            if path.len() < slot.get().len() {
                slot.insert(path);
            }
        }
        Entry::Vacant(slot) => {
            slot.insert(path);
        }
    }
}

fn remember_goal(
    bridges: &mut Vec<Vec<Token>>,
    longer: &mut Option<Vec<Token>>,
    prefix: &[Token],
    goal: Token,
) {
    let mut path = prefix.to_vec();
    path.push(goal);
    if path.len() < 3 || !distinct(&path) {
        return;
    }
    if path.len() == 3 {
        bridges.push(path);
        return;
    }
    let replace = longer
        .as_ref()
        .map(|kept| path.len() < kept.len())
        .unwrap_or(true);
    if replace {
        *longer = Some(path);
    }
}

fn token_path(parent: &HashMap<Token, Token>, start: Token, node: Token) -> Vec<Token> {
    let mut path = vec![node];
    let mut cursor = node;
    while cursor != start {
        let Some(prev) = parent.get(&cursor).copied() else {
            break;
        };
        if path.contains(&prev) {
            break;
        }
        path.push(prev);
        cursor = prev;
    }
    path.reverse();
    path
}

fn distinct(tokens: &[Token]) -> bool {
    let mut seen = HashSet::with_capacity(tokens.len());
    tokens.iter().all(|token| seen.insert(*token))
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

        fn outgoing(&self, token: Token, limit: usize) -> Option<Vec<Token>> {
            listed(&self.0, token, limit, true)
        }

        fn incoming(&self, token: Token, limit: usize) -> Option<Vec<Token>> {
            listed(&self.0, token, limit, false)
        }
    }

    fn listed(
        edges: &HashMap<(Token, Token), Vec<SlopeSegment>>,
        token: Token,
        limit: usize,
        forward: bool,
    ) -> Option<Vec<Token>> {
        let mut tokens: Vec<Token> = edges
            .iter()
            .filter(|((src, dst), segments)| {
                !segments.is_empty()
                    && if forward {
                        *src == token
                    } else {
                        *dst == token
                    }
            })
            .map(|((src, dst), _)| if forward { *dst } else { *src })
            .collect();
        tokens.sort();
        tokens.dedup();
        if tokens.len() > limit {
            None
        } else {
            Some(tokens)
        }
    }

    fn route_tokens(set: &RouteSet, route: &Route) -> Vec<Token> {
        let mut tokens = vec![set.edges[route.edges[0]].token_in];
        for index in &route.edges {
            tokens.push(set.edges[*index].token_out);
        }
        tokens
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

    #[test]
    fn a_common_neighbor_is_a_route_without_a_hub() {
        let query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(5)), vec![segment(1, 1, 1)]),
            ((token(5), token(2)), vec![segment(1, 1, 2)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(5), token(2)]
        );
    }

    #[test]
    fn a_chain_of_small_tokens_is_one_route() {
        let query = prepare(trade(&[(1, 10)], &[4]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(2), token(3)), vec![segment(1, 1, 2)]),
            ((token(3), token(4)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(2), token(3), token(4)]
        );
    }

    #[test]
    fn a_tendril_into_a_hub_joins_a_tendril_out() {
        let hub = token(9);
        let query = prepare(trade(&[(1, 10)], &[4]), &[hub], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(2), hub), vec![segment(1, 1, 2)]),
            ((hub, token(3)), vec![segment(1, 1, 3)]),
            ((token(3), token(4)), vec![segment(1, 1, 4)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(2), hub, token(3), token(4)]
        );
    }

    #[test]
    fn tendrils_around_two_hubs_include_the_hub_conduit() {
        let left = token(8);
        let right = token(9);
        let query = prepare(trade(&[(1, 10)], &[4]), &[left, right], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(2), left), vec![segment(1, 1, 2)]),
            ((left, right), vec![segment(1, 1, 3)]),
            ((right, token(3)), vec![segment(1, 1, 4)]),
            ((token(3), token(4)), vec![segment(1, 1, 5)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(2), left, right, token(3), token(4)]
        );
    }

    #[test]
    fn a_fat_bridge_is_probed_without_expanding_it() {
        let mut query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        query.neighbor_limit = 1;
        let fat = token(7);
        let catalog = Catalog(HashMap::from([
            ((token(1), fat), vec![segment(1, 1, 1)]),
            ((token(6), fat), vec![segment(1, 1, 2)]),
            ((fat, token(2)), vec![segment(1, 1, 3)]),
            ((fat, token(3)), vec![segment(1, 1, 4)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), fat, token(2)]
        );
        assert!(set.edges.iter().all(|edge| {
            edge.token_in != token(6)
                && edge.token_out != token(6)
                && edge.token_in != token(3)
                && edge.token_out != token(3)
        }));
    }

    #[test]
    fn two_local_bridges_are_both_routes() {
        let query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(5)), vec![segment(1, 1, 1)]),
            ((token(5), token(2)), vec![segment(1, 1, 2)]),
            ((token(1), token(6)), vec![segment(1, 1, 3)]),
            ((token(6), token(2)), vec![segment(1, 1, 4)]),
        ]));
        let set = execute(&query, &catalog);
        let mut found: Vec<Vec<Token>> = set
            .routes
            .iter()
            .map(|route| route_tokens(&set, route))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                vec![token(1), token(5), token(2)],
                vec![token(1), token(6), token(2)]
            ]
        );
    }

    #[test]
    fn the_direct_pair_stays_beside_a_local_bridge() {
        let query = prepare(trade(&[(1, 10)], &[2]), &[], None).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), token(2)), vec![segment(1, 1, 1)]),
            ((token(1), token(5)), vec![segment(1, 1, 2)]),
            ((token(5), token(2)), vec![segment(1, 1, 3)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 2);
        assert_eq!(route_tokens(&set, &set.routes[0]), vec![token(1), token(2)]);
    }

    #[test]
    fn a_wide_hub_joins_a_tendril_without_listing_its_conduits() {
        let hub = token(9);
        let mut edges = HashMap::new();
        edges.insert((token(1), token(2)), vec![segment(1, 1, 1)]);
        edges.insert((token(2), hub), vec![segment(1, 1, 2)]);
        edges.insert((hub, token(4)), vec![segment(1, 1, 3)]);
        for byte in 10..80_u8 {
            edges.insert((hub, token(byte)), vec![segment(1, 1, byte)]);
        }
        let query = prepare(trade(&[(1, 10)], &[4]), &[hub], None).unwrap();
        let set = execute(&query, &Catalog(edges));
        assert_eq!(set.routes.len(), 1);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![token(1), token(2), hub, token(4)]
        );
    }

    #[test]
    fn a_fat_neighbor_reaches_a_hub_one_conduit_past_it() {
        let fat = token(7);
        let hub = token(9);
        let mut query = prepare(trade(&[(1, 1)], &[4]), &[hub], None).unwrap();
        query.neighbor_limit = 1;
        let mut edges = HashMap::new();
        edges.insert((token(1), fat), vec![segment(1, 1, 1)]);
        edges.insert((fat, hub), vec![segment(1, 1, 2)]);
        edges.insert((hub, token(4)), vec![segment(1, 1, 3)]);
        for byte in 10..20_u8 {
            edges.insert((fat, token(byte)), vec![segment(1, 1, byte)]);
        }
        let set = execute(&query, &Catalog(edges));
        assert!(set
            .routes
            .iter()
            .any(|route| { route_tokens(&set, route) == vec![token(1), fat, hub, token(4)] }));
    }

    #[test]
    fn a_fat_predecessor_reaches_a_hub_one_conduit_before_it() {
        let fat = token(7);
        let hub = token(9);
        let mut query = prepare(trade(&[(1, 1)], &[4]), &[hub], None).unwrap();
        query.neighbor_limit = 1;
        let mut edges = HashMap::new();
        edges.insert((token(1), hub), vec![segment(1, 1, 1)]);
        edges.insert((hub, fat), vec![segment(1, 1, 2)]);
        edges.insert((fat, token(4)), vec![segment(1, 1, 3)]);
        for byte in 10..20_u8 {
            edges.insert((token(byte), fat), vec![segment(1, 1, byte)]);
        }
        let set = execute(&query, &Catalog(edges));
        assert!(set
            .routes
            .iter()
            .any(|route| { route_tokens(&set, route) == vec![token(1), hub, fat, token(4)] }));
    }

    #[test]
    fn a_native_hub_on_a_short_tendril_is_joined() {
        let mid = token(2);
        let faba = token(3);
        let end = token(4);
        let long_a = token(5);
        let long_b = token(6);
        let long_c = token(7);
        let query = prepare(trade(&[(1, 1)], &[4]), &[NATIVE, faba], Some(token(8))).unwrap();
        let catalog = Catalog(HashMap::from([
            ((token(1), mid), vec![segment(1, 1, 1)]),
            ((mid, NATIVE), vec![segment(1, 1, 2)]),
            ((NATIVE, faba), vec![segment(1, 1, 3)]),
            ((faba, end), vec![segment(1, 1, 4)]),
            ((mid, long_a), vec![segment(1, 1, 5)]),
            ((long_a, long_b), vec![segment(1, 1, 6)]),
            ((long_b, long_c), vec![segment(1, 1, 7)]),
            ((long_c, faba), vec![segment(1, 1, 8)]),
        ]));
        let set = execute(&query, &catalog);
        let found: Vec<Vec<Token>> = set
            .routes
            .iter()
            .map(|route| route_tokens(&set, route))
            .collect();
        assert!(
            found
                .iter()
                .any(|path| path == &vec![token(1), mid, NATIVE, faba, end]),
            "{found:?}"
        );
    }

    #[test]
    fn native_input_walks_onward_from_the_wrapped_token() {
        let wrapped = token(1);
        let query = prepare(
            Trade {
                inputs: vec![(NATIVE, amount(10))],
                outputs: vec![token(3)],
            },
            &[],
            Some(wrapped),
        )
        .unwrap();
        let catalog = Catalog(HashMap::from([
            ((wrapped, token(2)), vec![segment(1, 1, 1)]),
            ((token(2), token(3)), vec![segment(1, 1, 2)]),
        ]));
        let set = execute(&query, &catalog);
        assert_eq!(set.routes.len(), 1);
        assert_eq!(set.edges[0].curve, Curve::Wrap);
        assert_eq!(
            route_tokens(&set, &set.routes[0]),
            vec![NATIVE, wrapped, token(2), token(3)]
        );
    }
}
