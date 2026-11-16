use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

use alloy_primitives::{Address, B256};
use routing::SlopeSegment;
use searcher::{Conduits, Token};
use venue::{VenueRow, BALANCER_V2, UNI_V4};

use crate::graph::{Graph, PathEnds};
use crate::source::Source;

/// Time spent naming pool actions, including venue-row reads.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NameCost {
    pub calls: u64,
    pub us: u128,
    pub venue_calls: u64,
    pub venue_us: u128,
}

/// One venue-row read per kind and pool, shared by gas booking and problem building.
pub(crate) struct VenueMemory<'a, S> {
    inner: &'a S,
    rows: RefCell<HashMap<(u8, B256), Option<VenueRow>>>,
}

impl<'a, S> VenueMemory<'a, S> {
    pub(crate) fn new(inner: &'a S) -> Self {
        Self {
            inner,
            rows: RefCell::new(HashMap::new()),
        }
    }
}

impl<S: Source> Source for VenueMemory<'_, S> {
    fn tokens(&self) -> eyre::Result<Vec<venue::TokenRecord>> {
        self.inner.tokens()
    }

    fn visit_meta(
        &self,
        visit: &mut dyn FnMut(Address, Address, store::service::ConduitMeta) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        self.inner.visit_meta(visit)
    }

    fn conduit_segments(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<Vec<SlopeSegment>>> {
        self.inner.conduit_segments(token_in, token_out)
    }

    fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        let key = (kind, id);
        if let Some(hit) = self.rows.borrow().get(&key).cloned() {
            return Ok(hit);
        }
        let row = self.inner.read_venue(kind, id)?;
        self.rows.borrow_mut().insert(key, row.clone());
        Ok(row)
    }
}

pub(crate) fn catalog_swap(
    source: &impl Source,
    token_in: Token,
    token_out: Token,
    key: &routing::Action,
) -> Result<action::Action, trade::TradeError> {
    let mut cost = NameCost::default();
    catalog_swap_cost(source, token_in, token_out, key, &mut cost)
}

pub(crate) fn catalog_swap_cost(
    source: &impl Source,
    token_in: Token,
    token_out: Token,
    key: &routing::Action,
    cost: &mut NameCost,
) -> Result<action::Action, trade::TradeError> {
    let started = Instant::now();
    let venue = if matches!(key.kind, UNI_V4 | BALANCER_V2) {
        let read = Instant::now();
        let row = source.read_venue(key.kind, B256::from(key.pool));
        cost.venue_calls += 1;
        cost.venue_us += read.elapsed().as_micros();
        row.ok().flatten()
    } else {
        None
    };
    let action = venue::swap_action(token_in, token_out, key, venue.as_ref())?;
    cost.calls += 1;
    cost.us += started.elapsed().as_micros();
    Ok(action)
}

pub(crate) fn keep_disjoint<S: Source>(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    reader: &Reader<'_, S>,
    paths: &[PathEnds],
    limit: usize,
) -> Vec<PathEnds> {
    let mut kept = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        if kept.len() == limit {
            break;
        }
        if index.is_multiple_of(10) {
            eprintln!("pool check {index}/{}", paths.len());
        }
        if path_pools_ok(graph, incoming, reader, &path.path) {
            kept.push(path.clone());
        }
    }
    kept
}

pub(crate) fn path_pools_ok<S: Source>(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    reader: &Reader<'_, S>,
    path: &[Token],
) -> bool {
    if path.len() < 2 {
        return false;
    }
    let mut hops = Vec::new();
    for pair in path.windows(2) {
        let Some(pools) = pools_of(reader, pair[0], pair[1]) else {
            return false;
        };
        hops.push(pools);
    }
    for (index, hop) in hops.iter().enumerate() {
        for later in &hops[index + 1..] {
            if !hop.is_disjoint(later) {
                return false;
            }
        }
    }
    let Some(left) = incident_pools(graph, incoming, reader, path[0]) else {
        return false;
    };
    let Some(right) = incident_pools(graph, incoming, reader, path[path.len() - 1]) else {
        return false;
    };
    if !left.is_disjoint(&right) {
        return false;
    }
    let last = hops.len() - 1;
    !shares_foreign(&left, &hops, 0) && !shares_foreign(&right, &hops, last)
}

pub(crate) fn shares_foreign(
    side: &HashSet<[u8; 32]>,
    hops: &[HashSet<[u8; 32]>],
    own: usize,
) -> bool {
    hops.iter()
        .enumerate()
        .any(|(index, hop)| index != own && !side.is_disjoint(hop))
}

pub(crate) fn incident_pools<S: Source>(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    reader: &Reader<'_, S>,
    token: Token,
) -> Option<HashSet<[u8; 32]>> {
    let indeg = incoming.get(&token).map(|preds| preds.len()).unwrap_or(0);
    if graph.degree(token) + indeg > 12 {
        return None;
    }
    let mut pools = HashSet::new();
    if let Some(links) = graph.out.get(&token) {
        for link in links {
            pools.extend(pools_of(reader, token, link.to)?);
        }
    }
    if let Some(preds) = incoming.get(&token) {
        for pred in preds {
            pools.extend(pools_of(reader, *pred, token)?);
        }
    }
    Some(pools)
}

pub(crate) fn pools_of<S: Source>(
    reader: &Reader<'_, S>,
    from: Token,
    to: Token,
) -> Option<HashSet<[u8; 32]>> {
    let segments = reader.conduit(from, to)?;
    let pools: HashSet<_> = segments.iter().map(|segment| segment.action.pool).collect();
    if pools.is_empty() {
        None
    } else {
        Some(pools)
    }
}

pub(crate) type CurveCache = HashMap<(Token, Token), Option<Vec<SlopeSegment>>>;

pub(crate) struct Reader<'a, S> {
    pub(crate) source: &'a S,
    pub(crate) cache: RefCell<CurveCache>,
    pub(crate) graph: &'a Graph,
    pub(crate) incoming: &'a HashMap<Token, Vec<Token>>,
}

impl<S: Source> searcher::Conduits for Reader<'_, S> {
    fn conduit(&self, token_in: Token, token_out: Token) -> Option<Vec<SlopeSegment>> {
        let key = (token_in, token_out);
        if let Some(hit) = self.cache.borrow().get(&key) {
            return hit.clone();
        }
        let loaded = self
            .source
            .conduit_segments(
                Address::from_slice(&token_in.0),
                Address::from_slice(&token_out.0),
            )
            .ok()
            .flatten();
        self.cache.borrow_mut().insert(key, loaded.clone());
        loaded
    }

    fn outgoing(&self, token: Token, limit: usize) -> Option<Vec<Token>> {
        if self.graph.degree(token) > limit {
            return None;
        }
        let mut tokens: Vec<Token> = self.graph.neighbors(token).into_iter().collect();
        tokens.sort();
        if tokens.len() > limit {
            None
        } else {
            Some(tokens)
        }
    }

    fn incoming(&self, token: Token, limit: usize) -> Option<Vec<Token>> {
        let Some(preds) = self.incoming.get(&token) else {
            return Some(Vec::new());
        };
        if preds.len() > limit {
            None
        } else {
            Some(preds.clone())
        }
    }
}
