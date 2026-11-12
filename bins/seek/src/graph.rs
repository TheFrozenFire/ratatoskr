//! Directed conduit graph used to choose trades.
//!
//! An edge is one stored pair conduit. Liquidity is the sum of its piece
//! widths. Distance counts conduits. The first hub in the list wins when two
//! hubs are the same distance away.

use std::cell::Cell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use searcher::Token;

#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub to: Token,
    pub capacity: u128,
}

#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub kinds: u32,
    pub capacity: u128,
}

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

/// Who has a conduit into this token, in token order.
pub fn incoming(graph: &Graph) -> HashMap<Token, Vec<Token>> {
    let mut incoming: HashMap<Token, Vec<Token>> = HashMap::new();
    let mut sources: Vec<Token> = graph.out.keys().copied().collect();
    sources.sort();
    for from in sources {
        for next in targets(graph, from) {
            incoming.entry(next).or_default().push(from);
        }
    }
    for preds in incoming.values_mut() {
        preds.sort();
        preds.dedup();
    }
    incoming
}

/// The one hub among this token's counterparties, when there is exactly one.
pub fn sole_hub(
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

#[derive(Clone, Debug)]
pub struct PathEnds {
    pub path: Vec<Token>,
    pub kinds: u32,
    pub tendril_kinds: u32,
    pub min_capacity: u128,
    pub cross_hub: bool,
}

#[derive(Clone, Debug)]
pub struct Deep {
    pub token: Token,
    pub hub: Token,
    pub dist: u32,
    pub path: Vec<Token>,
    pub kinds: u32,
    pub min_capacity: u128,
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub max_to_hub: u32,
    pub max_from_hub: u32,
    pub max_to_hub_small: u32,
    pub max_from_hub_small: u32,
    pub to_hist: BTreeMap<u32, u64>,
    pub from_hist: BTreeMap<u32, u64>,
    pub far_to_kinds: u32,
    pub far_from_kinds: u32,
    pub reached_both: u64,
    pub only_toward_hub: u64,
    pub only_from_hub: u64,
    pub outside: u64,
    pub outside_edges: u64,
    pub outside_span: u32,
    pub deep_to: Vec<Deep>,
    pub deep_from: Vec<Deep>,
    pub via_hub: Vec<PathEnds>,
    pub long: Vec<PathEnds>,
    pub isolated: Vec<PathEnds>,
    pub wide_skips: u32,
}

struct Walk {
    dist: HashMap<Token, u32>,
    prev: HashMap<Token, Token>,
    origin: HashMap<Token, Token>,
}

struct Ctx<'a> {
    graph: &'a Graph,
    incoming: &'a HashMap<Token, Vec<Token>>,
    hubs: &'a HashSet<Token>,
    hub_order: &'a [Token],
    toward: &'a Walk,
    away: &'a Walk,
    to_hub: &'a [Walk],
    from_hub: &'a [Walk],
    wide_skips: &'a Cell<u32>,
}

#[derive(Clone)]
struct Tendril {
    token: Token,
    hub: Token,
    dist: u32,
    kinds: u32,
    capacity: u128,
}

struct Draft {
    path: Vec<Token>,
    tendril_kinds: u32,
    min_capacity: u128,
}

/// Trades whose catalog path is longer than the searcher, plus the one-hub pairs it does return.
pub fn select(graph: &Graph, hubs: &[Token]) -> Selection {
    let hub_set: HashSet<Token> = hubs.iter().copied().collect();
    let incoming = incoming(graph);
    let mut indegree = HashMap::new();
    for (token, preds) in &incoming {
        indegree.insert(*token, preds.len());
    }
    let outward = forward(graph);
    let toward = walk(hubs, &incoming);
    let away = walk(hubs, &outward);
    let to_tendrils = collect(graph, &toward, &indegree, &hub_set, true);
    let from_tendrils = collect(graph, &away, &indegree, &hub_set, false);

    let mut selection = Selection {
        max_to_hub: max_dist(&toward, &hub_set, false, graph, &indegree),
        max_from_hub: max_dist(&away, &hub_set, false, graph, &indegree),
        max_to_hub_small: max_dist(&toward, &hub_set, true, graph, &indegree),
        max_from_hub_small: max_dist(&away, &hub_set, true, graph, &indegree),
        to_hist: hist(&to_tendrils),
        from_hist: hist(&from_tendrils),
        far_to_kinds: far_kinds(&to_tendrils),
        far_from_kinds: far_kinds(&from_tendrils),
        deep_to: deep_paths(graph, &toward, &to_tendrils, true),
        deep_from: deep_paths(graph, &away, &from_tendrils, false),
        ..Selection::default()
    };
    count_reach(graph, &incoming, &toward, &away, &hub_set, &mut selection);

    let sources = diversify(&to_tendrils, |item| item.dist >= 2, 48);
    let dests = diversify(&from_tendrils, |item| item.dist >= 2, 48);
    let to_hub: Vec<Walk> = hubs
        .iter()
        .map(|hub| walk(std::slice::from_ref(hub), &incoming))
        .collect();
    let from_hub: Vec<Walk> = hubs
        .iter()
        .map(|hub| walk(std::slice::from_ref(hub), &outward))
        .collect();
    let wide_skips = Cell::new(0);
    let ctx = Ctx {
        graph,
        incoming: &incoming,
        hubs: &hub_set,
        hub_order: hubs,
        toward: &toward,
        away: &away,
        to_hub: &to_hub,
        from_hub: &from_hub,
        wide_skips: &wide_skips,
    };
    let mut used = HashSet::new();
    let mut attempts = 0_usize;
    selection.long.extend(take_pairs(
        &ctx,
        &sources,
        &dests,
        false,
        24,
        &mut used,
        &mut attempts,
    ));
    selection.long.extend(take_pairs(
        &ctx,
        &sources,
        &dests,
        true,
        8,
        &mut used,
        &mut attempts,
    ));
    selection.long.extend(extra_into(
        &ctx,
        &to_tendrils,
        &from_tendrils,
        &selection.long,
        &mut used,
    ));
    selection.wide_skips = wide_skips.get();
    selection.long.sort_by(|left, right| {
        right
            .path
            .len()
            .cmp(&left.path.len())
            .then(left.path.cmp(&right.path))
    });

    let via_sources = diversify(
        &to_tendrils,
        |item| item.dist == 1 && via_end(graph, &incoming, &hub_set, item, true),
        24,
    );
    let via_dests = diversify(
        &from_tendrils,
        |item| item.dist == 1 && via_end(graph, &incoming, &hub_set, item, false),
        24,
    );
    selection.via_hub = take_via(graph, &incoming, &hub_set, &via_sources, &via_dests);

    let outside = outside_tokens(graph, &incoming, &toward, &away);
    let (outside_edges, outside_span) = outside_span(graph, &outside);
    selection.outside_edges = outside_edges;
    selection.outside_span = outside_span;
    selection.isolated = isolated(graph, &incoming, &indegree, &hub_set, &outside);
    selection
}

fn forward(graph: &Graph) -> HashMap<Token, Vec<Token>> {
    let mut out = HashMap::new();
    let mut sources: Vec<Token> = graph.out.keys().copied().collect();
    sources.sort();
    for token in sources {
        out.insert(token, targets(graph, token));
    }
    out
}

fn walk(roots: &[Token], next: &HashMap<Token, Vec<Token>>) -> Walk {
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

fn ascend(walk: &Walk, token: Token) -> Option<Vec<Token>> {
    if !walk.dist.contains_key(&token) {
        return None;
    }
    let mut path = vec![token];
    let mut cursor = token;
    for _ in 0..64 {
        if walk.dist.get(&cursor).copied() == Some(0) {
            return Some(path);
        }
        let next = *walk.prev.get(&cursor)?;
        if path.contains(&next) {
            return None;
        }
        path.push(next);
        cursor = next;
    }
    None
}

fn directed_path(walk: &Walk, token: Token, ascend_follows_edges: bool) -> Option<Vec<Token>> {
    let path = ascend(walk, token)?;
    if ascend_follows_edges {
        Some(path)
    } else {
        let mut reversed = path;
        reversed.reverse();
        Some(reversed)
    }
}

fn measure(graph: &Graph, path: &[Token]) -> Option<(u32, u128)> {
    if path.len() < 2 {
        return None;
    }
    let mut kinds = 0_u32;
    let mut min_capacity = u128::MAX;
    for pair in path.windows(2) {
        let meta = graph.meta(pair[0], pair[1])?;
        kinds |= meta.kinds;
        min_capacity = min_capacity.min(meta.capacity);
    }
    Some((kinds, min_capacity))
}

fn tendril_kinds(graph: &Graph, path: &[Token], hubs: &HashSet<Token>) -> u32 {
    let mut kinds = 0_u32;
    for pair in path.windows(2) {
        if hubs.contains(&pair[0]) && hubs.contains(&pair[1]) {
            continue;
        }
        if let Some(meta) = graph.meta(pair[0], pair[1]) {
            kinds |= meta.kinds;
        }
    }
    kinds
}

fn crosses(path: &[Token], hubs: &HashSet<Token>) -> bool {
    path.windows(2)
        .any(|pair| hubs.contains(&pair[0]) && hubs.contains(&pair[1]))
}

fn simple(path: &[Token]) -> bool {
    let mut seen = HashSet::new();
    path.iter().all(|token| seen.insert(*token))
}

fn native(token: Token) -> bool {
    token.0 == [0_u8; 20]
}

fn small(graph: &Graph, indegree: &HashMap<Token, usize>, token: Token) -> bool {
    let incoming = indegree.get(&token).copied().unwrap_or(0);
    graph.degree(token) + incoming <= 6 && graph.capacity(token) < (1_u128 << 80)
}

fn collect(
    graph: &Graph,
    walk: &Walk,
    indegree: &HashMap<Token, usize>,
    hubs: &HashSet<Token>,
    ascend_follows_edges: bool,
) -> Vec<Tendril> {
    let mut tokens: Vec<Token> = walk.dist.keys().copied().collect();
    tokens.sort();
    let mut tendrils = Vec::new();
    for token in tokens {
        let dist = walk.dist[&token];
        if dist == 0 || hubs.contains(&token) || native(token) {
            continue;
        }
        if !small(graph, indegree, token) {
            continue;
        }
        let Some(path) = directed_path(walk, token, ascend_follows_edges) else {
            continue;
        };
        if path.len() as u32 != dist + 1 {
            continue;
        }
        let Some((kinds, _)) = measure(graph, &path) else {
            continue;
        };
        let Some(hub) = walk.origin.get(&token).copied() else {
            continue;
        };
        tendrils.push(Tendril {
            token,
            hub,
            dist,
            kinds,
            capacity: graph.capacity(token),
        });
    }
    tendrils
}

fn hist(tendrils: &[Tendril]) -> BTreeMap<u32, u64> {
    let mut counts = BTreeMap::new();
    for tendril in tendrils {
        *counts.entry(tendril.dist).or_default() += 1;
    }
    counts
}

fn far_kinds(tendrils: &[Tendril]) -> u32 {
    tendrils
        .iter()
        .filter(|tendril| tendril.dist >= 4)
        .fold(0_u32, |bits, tendril| bits | tendril.kinds)
}

fn max_dist(
    walk: &Walk,
    hubs: &HashSet<Token>,
    small_only: bool,
    graph: &Graph,
    indegree: &HashMap<Token, usize>,
) -> u32 {
    walk.dist
        .iter()
        .filter(|(token, _)| {
            !hubs.contains(token)
                && !native(**token)
                && (!small_only || small(graph, indegree, **token))
        })
        .map(|(_, dist)| *dist)
        .max()
        .unwrap_or(0)
}

fn count_reach(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    toward: &Walk,
    away: &Walk,
    hubs: &HashSet<Token>,
    selection: &mut Selection,
) {
    let mut all = HashSet::new();
    for token in graph.out.keys() {
        all.insert(*token);
    }
    for token in incoming.keys() {
        all.insert(*token);
    }
    for token in all {
        if hubs.contains(&token) || native(token) {
            continue;
        }
        let to = toward.dist.contains_key(&token);
        let from = away.dist.contains_key(&token);
        match (to, from) {
            (true, true) => selection.reached_both += 1,
            (true, false) => selection.only_toward_hub += 1,
            (false, true) => selection.only_from_hub += 1,
            (false, false) => selection.outside += 1,
        }
    }
}

fn deep_paths(
    graph: &Graph,
    walk: &Walk,
    tendrils: &[Tendril],
    ascend_follows_edges: bool,
) -> Vec<Deep> {
    let mut by_dist: BTreeMap<u32, Vec<&Tendril>> = BTreeMap::new();
    for tendril in tendrils {
        if tendril.dist >= 2 {
            by_dist.entry(tendril.dist).or_default().push(tendril);
        }
    }
    let dists: Vec<u32> = by_dist.keys().copied().collect();
    let start = dists.len().saturating_sub(8);
    let mut found = Vec::new();
    for dist in &dists[start..] {
        let mut at = by_dist[dist].clone();
        at.sort_by(|left, right| {
            left.capacity
                .cmp(&right.capacity)
                .then(left.token.cmp(&right.token))
        });
        let Some(low) = at.first() else {
            continue;
        };
        if let Some(deep) = deep_one(graph, walk, low, ascend_follows_edges) {
            found.push(deep);
        }
        if let Some(other) = at.iter().find(|tendril| tendril.kinds != low.kinds) {
            if let Some(deep) = deep_one(graph, walk, other, ascend_follows_edges) {
                found.push(deep);
            }
        }
    }
    found
}

fn deep_one(
    graph: &Graph,
    walk: &Walk,
    tendril: &Tendril,
    ascend_follows_edges: bool,
) -> Option<Deep> {
    let path = directed_path(walk, tendril.token, ascend_follows_edges)?;
    let (kinds, min_capacity) = measure(graph, &path)?;
    Some(Deep {
        token: tendril.token,
        hub: tendril.hub,
        dist: tendril.dist,
        path,
        kinds,
        min_capacity,
    })
}

fn diversify(tendrils: &[Tendril], keep: impl Fn(&Tendril) -> bool, limit: usize) -> Vec<Tendril> {
    let pool: Vec<&Tendril> = tendrils.iter().filter(|tendril| keep(tendril)).collect();
    let mut picked = Vec::new();
    let mut covered = 0_u32;
    let mut dist_count: HashMap<u32, usize> = HashMap::new();
    while picked.len() < limit {
        let mut best_index = None;
        let mut best_score = None;
        for (index, tendril) in pool.iter().enumerate() {
            if picked
                .iter()
                .any(|kept: &Tendril| kept.token == tendril.token)
            {
                continue;
            }
            let novel = (tendril.kinds & !covered).count_ones();
            let dist_need = u32::from(dist_count.get(&tendril.dist).copied().unwrap_or(0) < 3);
            let score = (
                novel,
                dist_need,
                Reverse(tendril.capacity),
                Reverse(tendril.dist),
                tendril.token,
            );
            if best_score.map(|kept| score > kept).unwrap_or(true) {
                best_score = Some(score);
                best_index = Some(index);
            }
        }
        let Some(index) = best_index else {
            break;
        };
        let tendril = (*pool[index]).clone();
        covered |= tendril.kinds;
        *dist_count.entry(tendril.dist).or_default() += 1;
        picked.push(tendril);
    }
    picked
}

fn counterparties(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    token: Token,
    hubs: &HashSet<Token>,
) -> HashSet<Token> {
    let mut set = graph.neighbors(token);
    if let Some(preds) = incoming.get(&token) {
        set.extend(preds.iter().copied());
    }
    set.retain(|item| !hubs.contains(item));
    set
}

fn via_end(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    tendril: &Tendril,
    toward_hub: bool,
) -> bool {
    if sole_hub(graph, incoming, tendril.token, hubs) != Some(tendril.hub) {
        return false;
    }
    if toward_hub {
        graph.meta(tendril.token, tendril.hub).is_some()
    } else {
        graph.meta(tendril.hub, tendril.token).is_some()
    }
}

fn take_via(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    sources: &[Tendril],
    dests: &[Tendril],
) -> Vec<PathEnds> {
    let mut drafts = Vec::new();
    for source in sources {
        for dest in dests {
            if source.hub != dest.hub || source.token == dest.token {
                continue;
            }
            if graph.meta(source.token, dest.token).is_some() {
                continue;
            }
            if !counterparties(graph, incoming, source.token, hubs)
                .is_disjoint(&counterparties(graph, incoming, dest.token, hubs))
            {
                continue;
            }
            let path = vec![source.token, source.hub, dest.token];
            if !simple(&path) {
                continue;
            }
            let Some((kinds, min_capacity)) = measure(graph, &path) else {
                continue;
            };
            drafts.push(PathEnds {
                tendril_kinds: kinds,
                path,
                kinds,
                min_capacity,
                cross_hub: false,
            });
        }
    }
    let mut picked = Vec::new();
    let mut covered = 0_u32;
    let mut used = HashSet::new();
    while picked.len() < 16 {
        let mut best = None;
        let mut best_score = None;
        for (index, draft) in drafts.iter().enumerate() {
            let source = draft.path[0];
            let dest = draft.path[2];
            if used.contains(&source) || used.contains(&dest) {
                continue;
            }
            let novel = (draft.kinds & !covered).count_ones();
            let score = (novel, Reverse(draft.min_capacity), source, dest);
            if best_score.map(|kept| score > kept).unwrap_or(true) {
                best_score = Some(score);
                best = Some(index);
            }
        }
        let Some(index) = best else {
            break;
        };
        let draft = drafts[index].clone();
        used.insert(draft.path[0]);
        used.insert(draft.path[2]);
        covered |= draft.kinds;
        picked.push(draft);
    }
    picked
}

fn take_pairs(
    ctx: &Ctx<'_>,
    sources: &[Tendril],
    dests: &[Tendril],
    cross_only: bool,
    limit: usize,
    used: &mut HashSet<Token>,
    attempts: &mut usize,
) -> Vec<PathEnds> {
    let mut drafts = Vec::new();
    for source in sources {
        for dest in dests {
            if source.token == dest.token {
                continue;
            }
            if cross_only {
                if source.hub == dest.hub {
                    continue;
                }
            } else if source.hub != dest.hub {
                continue;
            }
            if !disjoint_ends(ctx, source.token, dest.token) {
                continue;
            }
            if ctx.graph.meta(source.token, dest.token).is_some() {
                continue;
            }
            let Some(path) = join(ctx, source, dest, cross_only) else {
                continue;
            };
            if path.len() < 5 {
                continue;
            }
            let Some((_, min_capacity)) = measure(ctx.graph, &path) else {
                continue;
            };
            drafts.push(Draft {
                tendril_kinds: tendril_kinds(ctx.graph, &path, ctx.hubs),
                path,
                min_capacity,
            });
        }
    }
    pick_drafts(ctx, drafts, limit, used, attempts)
}

fn disjoint_ends(ctx: &Ctx<'_>, source: Token, dest: Token) -> bool {
    counterparties(ctx.graph, ctx.incoming, source, ctx.hubs).is_disjoint(&counterparties(
        ctx.graph,
        ctx.incoming,
        dest,
        ctx.hubs,
    ))
}

fn join(ctx: &Ctx<'_>, source: &Tendril, dest: &Tendril, cross: bool) -> Option<Vec<Token>> {
    let mut path = ascend(ctx.toward, source.token)?;
    if path.last().copied() != Some(source.hub) {
        return None;
    }
    let mut down = ascend(ctx.away, dest.token)?;
    if down.last().copied() != Some(dest.hub) {
        return None;
    }
    down.pop();
    down.reverse();
    if cross {
        ctx.graph.meta(source.hub, dest.hub)?;
        path.push(dest.hub);
    }
    path.extend(down);
    simple(&path).then_some(path)
}

fn pick_drafts(
    ctx: &Ctx<'_>,
    drafts: Vec<Draft>,
    limit: usize,
    used: &mut HashSet<Token>,
    attempts: &mut usize,
) -> Vec<PathEnds> {
    let mut taken = vec![false; drafts.len()];
    let mut covered = 0_u32;
    let mut chosen = Vec::new();
    while chosen.len() < limit && *attempts < 120 {
        let mut best = None;
        let mut best_score = None;
        for (index, draft) in drafts.iter().enumerate() {
            if taken[index] {
                continue;
            }
            let Some((source, dest)) = ends(&draft.path) else {
                continue;
            };
            if used.contains(&source) || used.contains(&dest) {
                continue;
            }
            let novel = (draft.tendril_kinds & !covered).count_ones();
            let score = (
                novel > 0,
                draft.path.len(),
                novel,
                Reverse(draft.min_capacity),
                source,
                dest,
            );
            if best_score.map(|kept| score > kept).unwrap_or(true) {
                best_score = Some(score);
                best = Some(index);
            }
        }
        let Some(index) = best else {
            break;
        };
        taken[index] = true;
        *attempts += 1;
        let Some(found) = confirm(ctx, &drafts[index]) else {
            continue;
        };
        let Some((source, dest)) = ends(&found.path) else {
            continue;
        };
        if !used.insert(source) {
            continue;
        }
        if !used.insert(dest) {
            used.remove(&source);
            continue;
        }
        covered |= found.tendril_kinds;
        chosen.push(found);
    }
    chosen
}

enum Reach {
    Path(Vec<Token>),
    Clear,
    Wide,
}

fn confirm(ctx: &Ctx<'_>, draft: &Draft) -> Option<PathEnds> {
    let length = draft.path.len().saturating_sub(1);
    let source = *draft.path.first()?;
    let dest = *draft.path.last()?;
    let free = avoiding(ctx, source, dest, length);
    if matches!(free, Reach::Wide) {
        ctx.wide_skips.set(ctx.wide_skips.get().saturating_add(1));
        return None;
    }
    let (via_len, via_path) = mediated(ctx, source, dest, length);
    let path = shortest_built(&free, via_len, via_path)?;
    if path.len() < 5 {
        return None;
    }
    let (kinds, min_capacity) = measure(ctx.graph, &path)?;
    Some(PathEnds {
        tendril_kinds: tendril_kinds(ctx.graph, &path, ctx.hubs),
        cross_hub: crosses(&path, ctx.hubs),
        path,
        kinds,
        min_capacity,
    })
}

/// Shortest path that never enters a hub, within `max_edges` conduits.
/// `Wide` means the neighborhood outgrew the search, so the length is unknown.
fn avoiding(ctx: &Ctx<'_>, source: Token, dest: Token, max_edges: usize) -> Reach {
    if source == dest {
        return Reach::Clear;
    }
    let mut parent = HashMap::new();
    let mut depth = HashMap::new();
    let mut queue = VecDeque::new();
    parent.insert(source, source);
    depth.insert(source, 0_usize);
    queue.push_back(source);
    let mut seen = 0_usize;
    while let Some(node) = queue.pop_front() {
        let hops = depth[&node];
        if hops >= max_edges {
            continue;
        }
        for next in targets(ctx.graph, node) {
            if ctx.hubs.contains(&next) || parent.contains_key(&next) {
                continue;
            }
            parent.insert(next, node);
            depth.insert(next, hops + 1);
            seen += 1;
            if seen > 200_000 {
                return Reach::Wide;
            }
            if next == dest {
                return Reach::Path(rebuild(&parent, source, dest));
            }
            queue.push_back(next);
        }
    }
    Reach::Clear
}

/// Shortest path that touches a hub, plus that length when the path does not stitch.
fn mediated(
    ctx: &Ctx<'_>,
    source: Token,
    dest: Token,
    max_edges: usize,
) -> (Option<usize>, Option<Vec<Token>>) {
    let mut best_len: Option<usize> = None;
    let mut pairs = Vec::new();
    for (left, hub_in) in ctx.to_hub.iter().enumerate() {
        let Some(up) = hub_in.dist.get(&source).copied() else {
            continue;
        };
        for (right, hub_out) in ctx.from_hub.iter().enumerate() {
            let Some(down) = hub_out.dist.get(&dest).copied() else {
                continue;
            };
            let mid = if left == right {
                0
            } else if ctx
                .graph
                .meta(ctx.hub_order[left], ctx.hub_order[right])
                .is_some()
            {
                1
            } else {
                continue;
            };
            let total = up as usize + mid + down as usize;
            if total == 0 || total > max_edges {
                continue;
            }
            match best_len {
                Some(len) if total > len => continue,
                Some(len) if total < len => {
                    best_len = Some(total);
                    pairs.clear();
                }
                None => best_len = Some(total),
                Some(_) => {}
            }
            pairs.push((left, right));
        }
    }
    let Some(total) = best_len else {
        return (None, None);
    };
    for (left, right) in pairs {
        let Some(mut path) = ascend(&ctx.to_hub[left], source) else {
            continue;
        };
        if left != right {
            path.push(ctx.hub_order[right]);
        }
        let Some(mut down) = ascend(&ctx.from_hub[right], dest) else {
            continue;
        };
        down.pop();
        down.reverse();
        path.extend(down);
        if path.len() == total + 1 && simple(&path) {
            return (Some(total), Some(path));
        }
    }
    (Some(total), None)
}

fn shortest_built(
    free: &Reach,
    via_len: Option<usize>,
    via_path: Option<Vec<Token>>,
) -> Option<Vec<Token>> {
    let free_len = match free {
        Reach::Path(path) => Some(path.len().saturating_sub(1)),
        Reach::Clear | Reach::Wide => None,
    };
    let best = match (free_len, via_len) {
        (Some(left), Some(right)) => left.min(right),
        (Some(left), None) => left,
        (None, Some(right)) => right,
        (None, None) => return None,
    };
    if let Reach::Path(path) = free {
        if path.len().saturating_sub(1) == best {
            return Some(path.clone());
        }
    }
    via_path.filter(|path| path.len().saturating_sub(1) == best)
}

fn ends(path: &[Token]) -> Option<(Token, Token)> {
    Some((*path.first()?, *path.last()?))
}

fn extra_into(
    ctx: &Ctx<'_>,
    sources: &[Tendril],
    dests: &[Tendril],
    chosen: &[PathEnds],
    used: &mut HashSet<Token>,
) -> Vec<PathEnds> {
    let mut found = Vec::new();
    for path in chosen {
        if found.len() == 6 {
            break;
        }
        let Some(dest_token) = path.path.last().copied() else {
            continue;
        };
        let Some(dest) = dests.iter().find(|tendril| tendril.token == dest_token) else {
            continue;
        };
        let mut matches: Vec<&Tendril> = sources
            .iter()
            .filter(|source| {
                source.hub == dest.hub
                    && source.dist >= 2
                    && source.token != dest.token
                    && !used.contains(&source.token)
            })
            .collect();
        matches.sort_by(|left, right| {
            left.capacity
                .cmp(&right.capacity)
                .then(left.token.cmp(&right.token))
        });
        matches.truncate(12);
        for source in matches {
            if !disjoint_ends(ctx, source.token, dest.token) {
                continue;
            }
            if ctx.graph.meta(source.token, dest.token).is_some() {
                continue;
            }
            let Some(joined) = join(ctx, source, dest, false) else {
                continue;
            };
            if joined.len() < 5 {
                continue;
            }
            let draft = Draft {
                tendril_kinds: tendril_kinds(ctx.graph, &joined, ctx.hubs),
                path: joined,
                min_capacity: 0,
            };
            let Some(confirmed) = confirm(ctx, &draft) else {
                continue;
            };
            if !used.insert(source.token) {
                continue;
            }
            found.push(confirmed);
            break;
        }
    }
    found
}

fn outside_tokens(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    toward: &Walk,
    away: &Walk,
) -> Vec<Token> {
    let mut all = HashSet::new();
    for token in graph.out.keys() {
        all.insert(*token);
    }
    for token in incoming.keys() {
        all.insert(*token);
    }
    let mut outside: Vec<Token> = all
        .into_iter()
        .filter(|token| !toward.dist.contains_key(token) && !away.dist.contains_key(token))
        .collect();
    outside.sort();
    outside
}

fn outside_span(graph: &Graph, outside: &[Token]) -> (u64, u32) {
    if outside.is_empty() {
        return (0, 0);
    }
    let set: HashSet<Token> = outside.iter().copied().collect();
    let mut adjacency: HashMap<Token, Vec<Token>> = HashMap::new();
    let mut edges = 0_u64;
    for token in outside {
        let mut nexts = Vec::new();
        for next in targets(graph, *token) {
            if set.contains(&next) {
                edges += 1;
                nexts.push(next);
            }
        }
        if !nexts.is_empty() {
            adjacency.insert(*token, nexts);
        }
    }
    let mut longest = 0_u32;
    for source in outside {
        let mut depth = HashMap::new();
        let mut queue = VecDeque::new();
        depth.insert(*source, 0_u32);
        queue.push_back(*source);
        while let Some(node) = queue.pop_front() {
            let hops = depth[&node];
            if hops >= 16 {
                continue;
            }
            let Some(children) = adjacency.get(&node) else {
                continue;
            };
            for child in children {
                if depth.contains_key(child) {
                    continue;
                }
                let next_hops = hops + 1;
                depth.insert(*child, next_hops);
                longest = longest.max(next_hops);
                queue.push_back(*child);
            }
        }
    }
    (edges, longest)
}

fn isolated(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    indegree: &HashMap<Token, usize>,
    hubs: &HashSet<Token>,
    outside: &[Token],
) -> Vec<PathEnds> {
    if outside.is_empty() {
        return Vec::new();
    }
    let mut smalls: Vec<Token> = outside
        .iter()
        .copied()
        .filter(|token| !native(*token) && small(graph, indegree, *token))
        .collect();
    smalls.sort_by_key(|token| (graph.capacity(*token), *token));
    smalls.truncate(40);
    let small_set: HashSet<Token> = smalls.iter().copied().collect();
    let mut found = Vec::new();
    let mut used = HashSet::new();
    for source in &smalls {
        if found.len() == 4 || used.contains(source) {
            continue;
        }
        let Some(path) = farthest(graph, incoming, hubs, *source, &small_set, &used) else {
            continue;
        };
        let Some((kinds, min_capacity)) = measure(graph, &path) else {
            continue;
        };
        let dest = path[path.len() - 1];
        used.insert(*source);
        used.insert(dest);
        found.push(PathEnds {
            tendril_kinds: tendril_kinds(graph, &path, hubs),
            path,
            kinds,
            min_capacity,
            cross_hub: false,
        });
    }
    found
}

fn farthest(
    graph: &Graph,
    incoming: &HashMap<Token, Vec<Token>>,
    hubs: &HashSet<Token>,
    source: Token,
    smalls: &HashSet<Token>,
    used: &HashSet<Token>,
) -> Option<Vec<Token>> {
    let source_set = counterparties(graph, incoming, source, hubs);
    let mut parent = HashMap::new();
    let mut depth = HashMap::new();
    let mut queue = VecDeque::new();
    parent.insert(source, source);
    depth.insert(source, 0_usize);
    queue.push_back(source);
    let mut best: Option<(usize, Token)> = None;
    while let Some(node) = queue.pop_front() {
        let hops = depth[&node];
        if hops >= 8 {
            continue;
        }
        for next in targets(graph, node) {
            if parent.contains_key(&next) {
                continue;
            }
            parent.insert(next, node);
            let next_hops = hops + 1;
            depth.insert(next, next_hops);
            if next_hops >= 4
                && smalls.contains(&next)
                && !used.contains(&next)
                && source_set.is_disjoint(&counterparties(graph, incoming, next, hubs))
            {
                let better = best
                    .map(|(dist, token)| next_hops > dist || (next_hops == dist && next < token))
                    .unwrap_or(true);
                if better {
                    best = Some((next_hops, next));
                }
            }
            queue.push_back(next);
        }
    }
    let (_, dest) = best?;
    Some(rebuild(&parent, source, dest))
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
}
