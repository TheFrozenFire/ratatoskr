//! Bounded walk from an endpoint that names few conduits.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet, VecDeque};

use action::Token;

use crate::route::{Build, Conduits};

/// Nodes the walk may enqueue from one endpoint.
const WALK_NODES: usize = 16_384;

/// Direct probes from a token the walk refuses to expand.
const FAT_PROBES: usize = 64;

impl<C: Conduits> Build<'_, C> {
    pub(crate) fn walk_routes(&mut self, from: Token, to: Token) -> Vec<Vec<usize>> {
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
