//! Backward fill of one basket count.
//!
//! A candidate count fixes the output each token must reach. Each step sends
//! the longest source that still sits on the current slope segment of every
//! edge, and keeps that span when its sink output per input beats the whole
//! budget. The longer prefix wins a tie. A prefix that would pass the
//! remaining demand is cut to the least input that meets the demand. Output
//! above the next edge's take stays on that token. Two routes that share an
//! edge add to one tender, and the output of that edge is the quote of the sum.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use action::Action;
use num_bigint::BigInt;
use num_traits::Zero;

use crate::{quote_curve, Chord, Curve, Edge, Flow, Plan, Problem, Route, Tender, Token};

pub(crate) fn output_bound(problem: &Problem) -> BigInt {
    let budgets: HashMap<Token, BigInt> = problem
        .inputs
        .iter()
        .map(|input| (input.token, input.amount.clone()))
        .collect();
    let mut bound: Option<BigInt> = None;
    for output in &problem.outputs {
        let mut cap = BigInt::from(0);
        for route in &problem.routes {
            if sink(problem, route) != Some(output.token) {
                continue;
            }
            let Some(source) = source(problem, route) else {
                continue;
            };
            let Some(budget) = budgets.get(&source) else {
                continue;
            };
            cap += forward(problem, route, budget);
        }
        let count = &cap / &output.weight;
        bound = Some(match bound {
            Some(current) if current < count => current,
            _ => count,
        });
    }
    bound.unwrap_or_else(BigInt::zero)
}

pub(crate) fn meet(problem: &Problem, count: &BigInt) -> Option<Plan> {
    meet_ranked(problem, count)
}

#[derive(Clone)]
struct Saved {
    step: Step,
    take: BigInt,
    need: BigInt,
    tenders: Vec<BigInt>,
}

fn meet_ranked(problem: &Problem, count: &BigInt) -> Option<Plan> {
    if count < &BigInt::from(0) {
        return None;
    }
    let mut state = Live::open(problem, count);
    let hubs: HashSet<Token> = problem.hubs.iter().copied().collect();
    let mut saved: Vec<Option<Saved>> = vec![None; problem.routes.len()];
    loop {
        if state.met() {
            break;
        }
        let room = gas_room(problem, &state.gas);
        let mut open = Vec::new();
        for (index, route) in problem.routes.iter().enumerate() {
            let sink = sink(problem, route)?;
            let need = state.remaining(&sink);
            if need.is_zero() {
                continue;
            }
            let budget = source_budget(problem, &state, index).unwrap_or_else(BigInt::zero);
            let step = if let Some(step) = reused_step(
                saved[index].as_ref(),
                problem,
                &state,
                index,
                &need,
                &budget,
                room.as_ref(),
            ) {
                step
            } else {
                let Some(step) = offer(problem, &state, index, &need, room.as_ref()) else {
                    saved[index] = None;
                    continue;
                };
                saved[index] = Some(Saved {
                    take: step.takes.first().cloned().unwrap_or_else(BigInt::zero),
                    need: need.clone(),
                    tenders: route
                        .edges
                        .iter()
                        .map(|edge| state.tender[*edge].clone())
                        .collect(),
                    step: step.clone(),
                });
                step
            };
            open.push(Candidate {
                index,
                hub: route_touches(route, problem, &hubs),
                step,
            });
        }
        let mut best: Option<Candidate> = None;
        for candidate in &open {
            if best
                .as_ref()
                .map(|held| candidate.beats(held))
                .unwrap_or(true)
            {
                best = Some(candidate.clone());
            }
        }
        let Some(mut candidate) = best else {
            break;
        };
        candidate = widen_opening(problem, &state, candidate, &open, room.as_ref());
        let touched = touched_tokens(problem, candidate.index, &candidate.step);
        let before: Vec<BigInt> = touched
            .iter()
            .map(|token| state.got.get(token).cloned().unwrap_or_else(BigInt::zero))
            .collect();
        let dirty_edges = dirty_edge_flags(problem, candidate.index, &candidate.step);
        state.apply(problem, candidate.index, &candidate.step);
        let mut dirty_tokens = HashSet::new();
        for (token, prev) in touched.iter().zip(before) {
            let now = state.got.get(token).cloned().unwrap_or_else(BigInt::zero);
            if now != prev {
                dirty_tokens.insert(*token);
            }
        }
        invalidate_saved(&mut saved, problem, &dirty_edges, &dirty_tokens);
    }
    Some(state.plan(problem))
}

fn reused_step(
    saved: Option<&Saved>,
    problem: &Problem,
    state: &Live,
    route_index: usize,
    need: &BigInt,
    budget: &BigInt,
    room: Option<&BigInt>,
) -> Option<Step> {
    let saved = saved?;
    if &saved.need != need && &saved.step.delivered >= need {
        return None;
    }
    let route = &problem.routes[route_index];
    if saved.tenders.len() != route.edges.len() {
        return None;
    }
    for (tender, edge_index) in saved.tenders.iter().zip(&route.edges) {
        if tender != &state.tender[*edge_index] {
            return None;
        }
    }
    if &saved.take > budget {
        return None;
    }
    let mut step = saved.step.clone();
    step.gas = step_gas(problem, state, route, &step.takes);
    if let Some(room) = room {
        if &step.gas > room {
            return None;
        }
    }
    if problem.gas_ceiling.is_some() && step.gas < saved.step.gas {
        return None;
    }
    Some(step)
}

fn touched_tokens(problem: &Problem, route_index: usize, step: &Step) -> Vec<Token> {
    let route = &problem.routes[route_index];
    let mut tokens = Vec::new();
    for (index, take) in step.takes.iter().enumerate() {
        if take.is_zero() {
            continue;
        }
        let edge = &problem.edges[route.edges[index]];
        if index > 0 {
            tokens.push(spent(edge));
        }
        if let Some(token) = edge.token_out() {
            tokens.push(token);
        }
    }
    tokens.sort();
    tokens.dedup();
    tokens
}

fn dirty_edge_flags(problem: &Problem, route_index: usize, step: &Step) -> Vec<bool> {
    let mut dirty = vec![false; problem.edges.len()];
    let route = &problem.routes[route_index];
    for (index, take) in step.takes.iter().enumerate() {
        if !take.is_zero() {
            dirty[route.edges[index]] = true;
        }
    }
    dirty
}

fn invalidate_saved(
    saved: &mut [Option<Saved>],
    problem: &Problem,
    dirty_edges: &[bool],
    dirty_tokens: &HashSet<Token>,
) {
    for (index, slot) in saved.iter_mut().enumerate() {
        if slot.is_none() {
            continue;
        }
        let stale = problem.routes[index].edges.iter().any(|edge_index| {
            dirty_edges[*edge_index] || dirty_tokens.contains(&spent(&problem.edges[*edge_index]))
        });
        if stale {
            *slot = None;
        }
    }
}

fn gas_room(problem: &Problem, spent: &BigInt) -> Option<BigInt> {
    let ceiling = problem.gas_ceiling.as_ref()?;
    if spent >= ceiling {
        Some(BigInt::from(0))
    } else {
        Some(ceiling - spent)
    }
}

struct Live {
    budget: HashMap<Token, BigInt>,
    got: HashMap<Token, BigInt>,
    demand: HashMap<Token, BigInt>,
    tender: Vec<BigInt>,
    route_in: Vec<BigInt>,
    route_out: Vec<BigInt>,
    opened: BTreeSet<Action>,
    gas: BigInt,
}

impl Live {
    fn open(problem: &Problem, count: &BigInt) -> Self {
        let mut demand = HashMap::new();
        for output in &problem.outputs {
            demand.insert(output.token, count * &output.weight);
        }
        Self {
            budget: problem
                .inputs
                .iter()
                .map(|input| (input.token, input.amount.clone()))
                .collect(),
            got: HashMap::new(),
            demand,
            tender: vec![BigInt::from(0); problem.edges.len()],
            route_in: vec![BigInt::from(0); problem.routes.len()],
            route_out: vec![BigInt::from(0); problem.routes.len()],
            opened: BTreeSet::new(),
            gas: BigInt::from(0),
        }
    }

    fn met(&self) -> bool {
        self.demand.iter().all(|(token, demand)| {
            self.got
                .get(token)
                .map(|got| got >= demand)
                .unwrap_or(false)
        })
    }

    fn remaining(&self, token: &Token) -> BigInt {
        let demand = self.demand.get(token).cloned().unwrap_or_else(BigInt::zero);
        let got = self.got.get(token).cloned().unwrap_or_else(BigInt::zero);
        if got >= demand {
            BigInt::from(0)
        } else {
            demand - got
        }
    }

    fn excess(&self, token: &Token) -> BigInt {
        let demand = self.demand.get(token).cloned().unwrap_or_else(BigInt::zero);
        let got = self.got.get(token).cloned().unwrap_or_else(BigInt::zero);
        if got > demand {
            got - demand
        } else {
            BigInt::from(0)
        }
    }

    fn apply(&mut self, problem: &Problem, route_index: usize, step: &Step) {
        let route = &problem.routes[route_index];
        let source = spent(&problem.edges[route.edges[0]]);
        if !step.takes[0].is_zero() {
            let budget = self.budget.entry(source).or_default();
            debug_assert!(*budget >= step.takes[0]);
            *budget -= &step.takes[0];
        }
        for (index, edge_index) in route.edges.iter().enumerate() {
            let take = &step.takes[index];
            if take.is_zero() {
                continue;
            }
            let edge = &problem.edges[*edge_index];
            let token_in = spent(edge);
            let token_out = paid(edge);
            if index > 0 {
                let got = self.got.entry(token_in).or_default();
                debug_assert!(*got >= *take);
                *got -= take;
            }
            let old = self.tender[*edge_index].clone();
            self.tender[*edge_index] += take;
            let actual = produce(edge, &old, take);
            *self.got.entry(token_out).or_default() += &actual;
            for (action, _) in paid_shares(edge, &old, take) {
                self.opened.insert(action);
            }
        }
        self.route_in[route_index] += &step.takes[0];
        self.route_out[route_index] += &step.delivered;
        self.gas += &step.gas;
    }

    fn plan(&self, problem: &Problem) -> Plan {
        let received: Vec<(Token, BigInt)> = problem
            .outputs
            .iter()
            .map(|output| {
                (
                    output.token,
                    self.got
                        .get(&output.token)
                        .cloned()
                        .unwrap_or_else(BigInt::zero),
                )
            })
            .collect();
        let spent: Vec<(Token, BigInt)> = problem
            .inputs
            .iter()
            .map(|input| {
                let left = self
                    .budget
                    .get(&input.token)
                    .cloned()
                    .unwrap_or_else(BigInt::zero);
                (input.token, &input.amount - &left)
            })
            .collect();
        let mut flows = Vec::new();
        for (index, (input, output)) in self.route_in.iter().zip(&self.route_out).enumerate() {
            if input.is_zero() && output.is_zero() {
                continue;
            }
            flows.push(Flow {
                route: index,
                input: input.clone(),
                output: output.clone(),
            });
        }
        Plan {
            count: basket(&problem.outputs, &received),
            flows,
            spent,
            received,
            tenders: tenders(problem, &self.tender),
            gas: self.gas.clone(),
        }
    }
}

fn basket(outputs: &[crate::Output], received: &[(Token, BigInt)]) -> BigInt {
    let legs: Vec<(BigInt, BigInt)> = outputs
        .iter()
        .map(|output| {
            let held = received
                .iter()
                .find(|(token, _)| *token == output.token)
                .map(|(_, amount)| amount.clone())
                .unwrap_or_else(BigInt::zero);
            (held, output.weight.clone())
        })
        .collect();
    routing::basket::basket_count(&legs).unwrap_or_else(BigInt::zero)
}

#[derive(Clone)]
struct Candidate {
    index: usize,
    hub: bool,
    step: Step,
}

impl Candidate {
    fn beats(&self, other: &Self) -> bool {
        if self.step.eff.free != other.step.eff.free {
            return self.step.eff.free;
        }
        if !self.same_slope(other) {
            return self.steeper_than(other);
        }
        self.tie_beats(other)
    }

    fn same_slope(&self, other: &Self) -> bool {
        &self.step.eff.num * &other.step.eff.den == &other.step.eff.num * &self.step.eff.den
    }

    fn steeper_than(&self, other: &Self) -> bool {
        &self.step.eff.num * &other.step.eff.den > &other.step.eff.num * &self.step.eff.den
    }

    fn tie_beats(&self, other: &Self) -> bool {
        if self.hub != other.hub {
            return self.hub;
        }
        if self.step.delivered != other.step.delivered {
            return self.step.delivered > other.step.delivered;
        }
        self.index < other.index
    }
}

#[derive(Clone)]
struct Step {
    takes: Vec<BigInt>,
    delivered: BigInt,
    gas: BigInt,
    eff: Eff,
}

#[derive(Clone)]
struct Eff {
    free: bool,
    num: BigInt,
    den: BigInt,
}

/// Source amounts at or below this are scored one by one.
const SCAN_LIMIT: i64 = 16_384;

fn offer(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    need: &BigInt,
    room: Option<&BigInt>,
) -> Option<Step> {
    if let Some(step) = free_offer(problem, state, route_index, need, room) {
        return Some(step);
    }
    paid_offer(problem, state, route_index, need, room)
}

fn free_offer(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    need: &BigInt,
    room: Option<&BigInt>,
) -> Option<Step> {
    let cap = sink_output(problem, state, route_index, &BigInt::from(0));
    if cap.is_zero() {
        return None;
    }
    let ask = std::cmp::min(need.clone(), cap);
    let step = try_ask(problem, state, route_index, &ask, room)?;
    if !step.eff.free || step.delivered.is_zero() {
        return None;
    }
    Some(step)
}

fn paid_offer(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    need: &BigInt,
    room: Option<&BigInt>,
) -> Option<Step> {
    let budget = source_budget(problem, state, route_index)?;
    if budget.is_zero() {
        return None;
    }
    let useful = source_limit(problem, state, route_index, &budget);
    if useful.is_zero() {
        return None;
    }
    let limit = gas_limit(problem, state, route_index, &useful, room)?;
    let (take, gain) = best_prefix(problem, state, route_index, &limit)?;
    let step = if &gain >= need {
        try_ask(problem, state, route_index, need, room)
            .or_else(|| push(problem, state, route_index, &take, room))
    } else {
        push(problem, state, route_index, &take, room)
    }?;
    if step.delivered.is_zero() {
        None
    } else {
        Some(step)
    }
}

fn best_prefix(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<(BigInt, BigInt)> {
    if let Some(take) = linear_source(problem, state, route_index, limit) {
        let gain = sink_output(problem, state, route_index, &take);
        if !gain.is_zero() {
            return Some((take, gain));
        }
    }
    if limit <= &BigInt::from(SCAN_LIMIT) {
        return scan_prefix(problem, state, route_index, limit);
    }
    exact_chord(problem, state, route_index, limit)
        .or_else(|| fallback_prefix(problem, state, route_index, limit))
}

fn scan_prefix(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<(BigInt, BigInt)> {
    let mut best_take: Option<BigInt> = None;
    let mut best_gain = BigInt::from(0);
    let mut take = BigInt::from(1);
    while &take <= limit {
        if let Some(gain) = spent_gain(problem, state, route_index, &take) {
            if ratio_beats(&gain, &take, &best_gain, best_take.as_ref()) {
                best_gain = gain;
                best_take = Some(take.clone());
            }
        }
        take += 1;
    }
    best_take.map(|take| (take, best_gain))
}

fn exact_chord(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<(BigInt, BigInt)> {
    let route = &problem.routes[route_index];
    if route.edges.len() != 1 {
        return None;
    }
    let edge_index = *route.edges.first()?;
    let Curve::Chords(segments) = &problem.edges.get(edge_index)?.curve else {
        return None;
    };
    let head = head(segments, &state.tender[edge_index])?;
    if !head.offset.is_zero() {
        return None;
    }
    let span = std::cmp::min(limit.clone(), head.rem_w);
    let divisor = gcd(head.output.clone(), head.width.clone());
    if divisor.is_zero() {
        return None;
    }
    let step = &head.width / &divisor;
    if step.is_zero() || step > span {
        return None;
    }
    let take = (&span / &step) * &step;
    spent_gain(problem, state, route_index, &take).map(|gain| (take, gain))
}

fn fallback_prefix(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<(BigInt, BigInt)> {
    let mut best_take = BigInt::from(1);
    let mut best_gain =
        spent_gain(problem, state, route_index, &best_take).unwrap_or_else(BigInt::zero);
    if problem.routes[route_index].edges.len() > 1 {
        // The opening unit has the best average while the slope falls. The
        // step is the rest of the slope segment that unit lands on. The whole
        // budget is quoted only when every edge it crosses keeps that slope.
        if let Some((take, gain)) = span_candidate(problem, state, route_index, limit) {
            best_take = take;
            best_gain = gain;
        } else if let Some((take, gain)) = landing_candidate(problem, state, route_index, limit) {
            if ratio_beats(&gain, &take, &best_gain, Some(&best_take)) {
                best_take = take;
                best_gain = gain;
            }
        }
    } else if let Some((take, gain)) = head_candidate(problem, state, route_index, limit) {
        if ratio_beats(&gain, &take, &best_gain, Some(&best_take)) {
            best_take = take;
            best_gain = gain;
        }
    }
    if flat_through(problem, state, route_index, limit) {
        if let Some(gain) = spent_gain(problem, state, route_index, limit) {
            if ratio_beats(&gain, limit, &best_gain, Some(&best_take)) {
                best_take = limit.clone();
                best_gain = gain;
            }
        }
    }
    if best_gain.is_zero() {
        let need = BigInt::from(1);
        let take = least_source(problem, state, route_index, &need, limit)?;
        let gain = spent_gain(problem, state, route_index, &take)?;
        return Some((take, gain));
    }
    Some((best_take, best_gain))
}

/// Largest source that keeps every edge on its current slope segment.
fn span_candidate(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<(BigInt, BigInt)> {
    let one = BigInt::from(1);
    if limit < &one {
        return None;
    }
    let take = match span_take(problem, state, route_index, limit) {
        Some(take) if span_exact(problem, state, route_index, limit, &take) => take,
        _ if stays_on_heads(problem, state, route_index, &one) => {
            let hi = span_hi(problem, state, route_index, limit);
            if hi.is_zero() {
                return None;
            }
            span_search(problem, state, route_index, &hi)?
        }
        _ => return None,
    };
    let gain = spent_gain(problem, state, route_index, &take)?;
    Some((take, gain))
}

fn span_exact(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
    take: &BigInt,
) -> bool {
    if !stays_on_heads(problem, state, route_index, take) {
        return false;
    }
    take == limit || !stays_on_heads(problem, state, route_index, &(take + 1))
}

fn span_search(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<BigInt> {
    let one = BigInt::from(1);
    if stays_on_heads(problem, state, route_index, limit) {
        return Some(limit.clone());
    }
    let mut lo = one;
    let mut hi = limit.clone();
    while &lo + 1 < hi {
        let mid = (&lo + &hi) / 2;
        if stays_on_heads(problem, state, route_index, &mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

/// Largest source that can still sit on the first edge's current slope segment.
fn span_hi(problem: &Problem, state: &Live, route_index: usize, limit: &BigInt) -> BigInt {
    let Some(first) = problem.routes[route_index].edges.first() else {
        return limit.clone();
    };
    let Curve::Chords(segments) = &problem.edges[*first].curve else {
        return limit.clone();
    };
    let Some(head) = head(segments, &state.tender[*first]) else {
        return BigInt::from(0);
    };
    std::cmp::min(limit.clone(), head.rem_w)
}

/// Largest source that ends on the slope segment reached by one unit of flow.
///
/// One unit that has already left the opening segment still has a landing
/// segment. The step runs to the end of that segment when its average holds.
fn landing_candidate(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<(BigInt, BigInt)> {
    let one = BigInt::from(1);
    if limit < &one || problem.routes[route_index].edges.len() < 2 {
        return None;
    }
    let cap = landing_source(problem, state, route_index, &one, limit)?;
    let gain = spent_gain(problem, state, route_index, &cap)?;
    Some((cap, gain))
}

/// Grows a one-unit winner across the slope segments it still beats.
///
/// The opening unit still decides which route wins. Later segments join the
/// step while the first and last unit of each one stay strictly above the
/// next-best route. A shallower landing is left for that route.
fn widen_opening(
    problem: &Problem,
    state: &Live,
    mut candidate: Candidate,
    open: &[Candidate],
    room: Option<&BigInt>,
) -> Candidate {
    let Some(runner) = runner_up(open, candidate.index) else {
        return candidate;
    };
    if runner.step.eff.free || candidate.step.eff.free {
        return candidate;
    }
    if candidate.step.takes.first() != Some(&BigInt::from(1)) {
        return candidate;
    }
    let Some(sink) = sink(problem, &problem.routes[candidate.index]) else {
        return candidate;
    };
    let need = state.remaining(&sink);
    if candidate.step.delivered >= need {
        return candidate;
    }
    let Some(limit) = source_budget(problem, state, candidate.index) else {
        return candidate;
    };
    let mut take = BigInt::from(1);
    for _ in 0..4096 {
        // A cursor on a boundary has no room left in that segment. Probe the
        // next source unit so the following segment can join this step.
        let cap = match landing_source(problem, state, candidate.index, &take, &limit) {
            Some(cap) => cap,
            None => {
                let next = &take + 1;
                if next > limit {
                    break;
                }
                match landing_source(problem, state, candidate.index, &next, &limit) {
                    Some(cap) => cap,
                    None => break,
                }
            }
        };
        if cap <= take {
            break;
        }
        let Some(at_next) = unit_marginal(problem, state, candidate.index, &(&take + 1)) else {
            break;
        };
        let Some(at_end) = unit_marginal(problem, state, candidate.index, &cap) else {
            break;
        };
        if !steeper_than_rate(&at_next, runner) || !steeper_than_rate(&at_end, runner) {
            break;
        }
        let Some(pushed) = push(problem, state, candidate.index, &cap, room) else {
            break;
        };
        if pushed.delivered >= need {
            if let Some(cut) = try_ask(problem, state, candidate.index, &need, room) {
                if cut.takes.first().is_some_and(|taken| taken > &take) {
                    candidate.step = cut;
                }
            }
            break;
        }
        candidate.step = pushed;
        take = cap;
    }
    candidate
}

fn runner_up(open: &[Candidate], winner: usize) -> Option<&Candidate> {
    let mut best: Option<&Candidate> = None;
    for candidate in open {
        if candidate.index == winner {
            continue;
        }
        if best.is_some_and(|held| !candidate.beats(held)) {
            continue;
        }
        best = Some(candidate);
    }
    best
}

fn steeper_than_rate(marginal: &BigInt, runner: &Candidate) -> bool {
    marginal * &runner.step.eff.den > runner.step.eff.num
}

fn unit_marginal(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    at: &BigInt,
) -> Option<BigInt> {
    let end = spent_gain(problem, state, route_index, at)?;
    let prev = if at <= &BigInt::from(1) {
        BigInt::from(0)
    } else {
        spent_gain(problem, state, route_index, &(at - 1))?
    };
    if end <= prev {
        None
    } else {
        Some(end - prev)
    }
}

/// Largest source at or below `limit` that still ends on the slope segment `take` reaches.
fn landing_source(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    take: &BigInt,
    limit: &BigInt,
) -> Option<BigInt> {
    if take.is_zero() || problem.routes[route_index].edges.len() < 2 {
        return None;
    }
    let sim = simulate(problem, state, route_index, take)?;
    if sim.takes.first() != Some(take) {
        return None;
    }
    let route = &problem.routes[route_index];
    let mut cap = limit.clone();
    let mut bound = false;
    for (index, edge_index) in route.edges.iter().enumerate() {
        let edge = &problem.edges[*edge_index];
        let tender = &state.tender[*edge_index];
        let Some(room) = landing_room(edge, tender, &sim.takes[index]) else {
            continue;
        };
        if room.is_zero() {
            return None;
        }
        bound = true;
        if let Some(allowed) = source_for_enter(problem, state, route, index, &room) {
            if allowed < cap {
                cap = allowed;
            }
        }
    }
    if !bound || cap <= *take {
        None
    } else {
        Some(cap)
    }
}

/// Input from `tender` that reaches the end of the segment holding `tender + take`.
fn landing_room(edge: &Edge, tender: &BigInt, take: &BigInt) -> Option<BigInt> {
    let Curve::Chords(segments) = &edge.curve else {
        return None;
    };
    if take.is_zero() {
        return None;
    }
    let pos = tender + take;
    let mut cursor = BigInt::from(0);
    for segment in segments {
        if segment.width.is_zero() {
            continue;
        }
        let seg_end = &cursor + &segment.width;
        if pos <= seg_end {
            return Some(&seg_end - tender);
        }
        cursor = seg_end;
    }
    remaining_input(edge, tender)
}

/// The budget stays on one slope of every edge, so its average can tie the opening.
fn flat_through(problem: &Problem, state: &Live, route_index: usize, limit: &BigInt) -> bool {
    if limit.is_zero() {
        return false;
    }
    let route = &problem.routes[route_index];
    let mut flow = limit.clone();
    for (index, edge_index) in route.edges.iter().enumerate() {
        let edge = &problem.edges[*edge_index];
        let tender = &state.tender[*edge_index];
        let mut enter = flow.clone();
        if index > 0 {
            enter += state.excess(&spent(edge));
        }
        let take = match remaining_input(edge, tender) {
            Some(rem) => std::cmp::min(enter, rem),
            None => enter,
        };
        if take.is_zero() || !one_slope(edge, tender, &take) {
            return false;
        }
        flow = produce(edge, tender, &take);
    }
    true
}

fn one_slope(edge: &Edge, tender: &BigInt, take: &BigInt) -> bool {
    let Curve::Chords(segments) = &edge.curve else {
        return true;
    };
    let Some(head) = head(segments, tender) else {
        return false;
    };
    let mut skip = tender.clone();
    let mut left = take.clone();
    let mut first = true;
    for segment in segments {
        if segment.width.is_zero() {
            continue;
        }
        if skip >= segment.width {
            skip -= &segment.width;
            continue;
        }
        if !first && (&segment.output * &head.width != &head.output * &segment.width) {
            return false;
        }
        first = false;
        let room = &segment.width - &skip;
        if left <= room {
            return true;
        }
        left -= &room;
        skip = BigInt::from(0);
    }
    true
}

/// Source bound from the head of each edge, inverted back along the route.
fn span_take(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<BigInt> {
    let route = &problem.routes[route_index];
    let mut cap = limit.clone();
    for (index, edge_index) in route.edges.iter().enumerate() {
        let edge = &problem.edges[*edge_index];
        let tender = &state.tender[*edge_index];
        let Some(enter_cap) = enter_cap(edge, tender) else {
            continue;
        };
        if enter_cap.is_zero() {
            return None;
        }
        let Some(allowed) = source_for_enter(problem, state, route, index, &enter_cap) else {
            continue;
        };
        if allowed < cap {
            cap = allowed;
        }
    }
    if cap.is_zero() {
        None
    } else {
        Some(cap)
    }
}

/// Largest input that still lands on this edge's current slope segment.
///
/// `None` means a larger input cannot leave the segment.
fn enter_cap(edge: &Edge, tender: &BigInt) -> Option<BigInt> {
    let Curve::Chords(segments) = &edge.curve else {
        return None;
    };
    let remaining = remaining_input(edge, tender).unwrap_or_else(BigInt::zero);
    if remaining.is_zero() {
        return None;
    }
    let Some(head) = head(segments, tender) else {
        return Some(BigInt::from(0));
    };
    if remaining <= head.rem_w {
        None
    } else {
        Some(head.rem_w)
    }
}

/// Largest source whose flow into `index` is at most `enter_cap`.
///
/// `None` means the cap does not bind before the upstream edges are exhausted.
fn source_for_enter(
    problem: &Problem,
    state: &Live,
    route: &Route,
    index: usize,
    enter_cap: &BigInt,
) -> Option<BigInt> {
    let mut max_enter = enter_cap.clone();
    for cursor in (0..=index).rev() {
        if cursor == 0 {
            return Some(max_enter);
        }
        let edge_index = route.edges[cursor];
        let excess = state.excess(&spent(&problem.edges[edge_index]));
        let max_produce = if excess >= max_enter {
            BigInt::from(0)
        } else {
            &max_enter - &excess
        };
        let prev_index = route.edges[cursor - 1];
        let prev = &problem.edges[prev_index];
        let prev_tender = &state.tender[prev_index];
        let max_take = max_take_for_output(prev, prev_tender, &max_produce);
        match remaining_input(prev, prev_tender) {
            Some(rem) if max_take >= rem => return None,
            _ => max_enter = max_take,
        }
    }
    None
}

/// Largest take whose quote is at most `max_out`.
fn max_take_for_output(edge: &Edge, tender: &BigInt, max_out: &BigInt) -> BigInt {
    let need = max_out + 1;
    match least_take(edge, tender, &need) {
        Some(take) if take > BigInt::from(0) => take - 1,
        None => remaining_input(edge, tender).unwrap_or_else(|| max_out.clone()),
        Some(_) => BigInt::from(0),
    }
}

fn stays_on_heads(problem: &Problem, state: &Live, route_index: usize, source: &BigInt) -> bool {
    let sim = simulate(problem, state, route_index, source);
    let Some(sim) = sim else {
        return false;
    };
    if sim.takes.first() != Some(source) {
        return false;
    }
    let route = &problem.routes[route_index];
    sim.takes.iter().enumerate().all(|(index, take)| {
        let edge_index = route.edges[index];
        on_current_piece(&problem.edges[edge_index], &state.tender[edge_index], take)
    })
}

fn on_current_piece(edge: &Edge, tender: &BigInt, take: &BigInt) -> bool {
    if take.is_zero() {
        return true;
    }
    match &edge.curve {
        Curve::Unlimited => true,
        Curve::Chords(segments) => head(segments, tender).is_some_and(|head| take <= &head.rem_w),
    }
}

fn head_candidate(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<(BigInt, BigInt)> {
    let route = &problem.routes[route_index];
    if route.edges.len() != 1 {
        return None;
    }
    let edge_index = *route.edges.first()?;
    let Curve::Chords(segments) = &problem.edges.get(edge_index)?.curve else {
        return None;
    };
    let head = head(segments, &state.tender[edge_index])?;
    let take = std::cmp::min(limit.clone(), head.rem_w);
    if take.is_zero() {
        return None;
    }
    spent_gain(problem, state, route_index, &take).map(|gain| (take, gain))
}

fn ratio_beats(
    gain: &BigInt,
    take: &BigInt,
    best_gain: &BigInt,
    best_take: Option<&BigInt>,
) -> bool {
    let Some(best_take) = best_take else {
        return !gain.is_zero();
    };
    if gain.is_zero() {
        return false;
    }
    let left = gain * best_take;
    let right = best_gain * take;
    left > right || (left == right && take > best_take)
}

fn linear_source(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    limit: &BigInt,
) -> Option<BigInt> {
    let route = &problem.routes[route_index];
    let mut source_cap = limit.clone();
    let mut out_per_source = BigInt::from(1);
    for (index, edge_index) in route.edges.iter().enumerate() {
        let edge = &problem.edges[*edge_index];
        if index > 0 && !state.excess(&spent(edge)).is_zero() {
            return None;
        }
        match &edge.curve {
            Curve::Unlimited => {}
            Curve::Chords(segments) => {
                let head = head(segments, &state.tender[*edge_index])?;
                if !(&head.output % &head.width).is_zero() {
                    return None;
                }
                let rate = &head.output / &head.width;
                let allow = linear_input(segments, &state.tender[*edge_index], &rate);
                if allow.is_zero() || out_per_source.is_zero() {
                    return None;
                }
                let max_source = &allow / &out_per_source;
                if source_cap > max_source {
                    source_cap = max_source;
                }
                out_per_source *= rate;
            }
        }
    }
    if source_cap.is_zero() || &source_cap > limit {
        None
    } else {
        Some(source_cap)
    }
}

fn linear_input(segments: &[Chord], tender: &BigInt, rate: &BigInt) -> BigInt {
    let mut skip = tender.clone();
    let mut allow = BigInt::from(0);
    for segment in segments {
        if skip >= segment.width {
            skip -= &segment.width;
            continue;
        }
        let integer = (&segment.output % &segment.width).is_zero();
        let piece = &segment.output / &segment.width;
        if !integer || &piece != rate {
            break;
        }
        allow += &segment.width - &skip;
        skip = BigInt::from(0);
    }
    allow
}

fn source_budget(problem: &Problem, state: &Live, route_index: usize) -> Option<BigInt> {
    let route = problem.routes.get(route_index)?;
    let edge = problem.edges.get(*route.edges.first()?)?;
    Some(
        state
            .budget
            .get(&spent(edge))
            .cloned()
            .unwrap_or_else(BigInt::zero),
    )
}

fn source_limit(problem: &Problem, state: &Live, route_index: usize, budget: &BigInt) -> BigInt {
    let route = &problem.routes[route_index];
    let mut wanted: Option<BigInt> = None;
    for edge_index in route.edges.iter().rev() {
        let edge = &problem.edges[*edge_index];
        let tender = &state.tender[*edge_index];
        let width = remaining_input(edge, tender);
        let useful = match (&wanted, width) {
            (None, None) => budget.clone(),
            (None, Some(width)) => width,
            (Some(want), None) => want.clone(),
            (Some(want), Some(width)) => least_take(edge, tender, want).unwrap_or(width),
        };
        wanted = Some(useful);
    }
    match wanted {
        Some(useful) if &useful < budget => useful,
        _ => budget.clone(),
    }
}

fn gas_limit(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    useful: &BigInt,
    room: Option<&BigInt>,
) -> Option<BigInt> {
    let Some(room) = room else {
        return Some(useful.clone());
    };
    if &charge(problem, state, route_index, useful)? <= room {
        return Some(useful.clone());
    }
    let one = BigInt::from(1);
    if useful < &one || &charge(problem, state, route_index, &one)? > room {
        return None;
    }
    let mut lo = one;
    let mut hi = useful.clone();
    while &lo + 1 < hi {
        let mid = (&lo + &hi) / 2;
        if &charge(problem, state, route_index, &mid)? <= room {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    Some(lo)
}

fn charge(problem: &Problem, state: &Live, route_index: usize, source: &BigInt) -> Option<BigInt> {
    let sim = simulate(problem, state, route_index, source)?;
    let route = &problem.routes[route_index];
    Some(step_gas(problem, state, route, &sim.takes))
}

fn least_source(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    need: &BigInt,
    limit: &BigInt,
) -> Option<BigInt> {
    if &sink_output(problem, state, route_index, limit) < need {
        return None;
    }
    let mut lo = BigInt::from(0);
    let mut hi = limit.clone();
    while &lo + 1 < hi {
        let mid = (&lo + &hi) / 2;
        if &sink_output(problem, state, route_index, &mid) >= need {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    Some(hi)
}

fn spent_gain(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    source: &BigInt,
) -> Option<BigInt> {
    let sim = simulate(problem, state, route_index, source)?;
    if sim.takes.first() != Some(source) || sim.delivered.is_zero() {
        return None;
    }
    Some(sim.delivered)
}

fn sink_output(problem: &Problem, state: &Live, route_index: usize, source: &BigInt) -> BigInt {
    simulate(problem, state, route_index, source)
        .map(|sim| sim.delivered)
        .unwrap_or_else(BigInt::zero)
}

struct Sim {
    takes: Vec<BigInt>,
    delivered: BigInt,
}

fn simulate(problem: &Problem, state: &Live, route_index: usize, source: &BigInt) -> Option<Sim> {
    let route = &problem.routes[route_index];
    if route.edges.is_empty() {
        return None;
    }
    let mut carry = source.clone();
    let mut takes = Vec::with_capacity(route.edges.len());
    for (index, edge_index) in route.edges.iter().enumerate() {
        let edge = problem.edges.get(*edge_index)?;
        let mut enter = carry.clone();
        if index > 0 {
            enter += state.excess(&spent(edge));
        }
        if index == 0 {
            let budget = state
                .budget
                .get(&spent(edge))
                .cloned()
                .unwrap_or_else(BigInt::zero);
            if enter > budget {
                return None;
            }
        }
        let take = match remaining_input(edge, &state.tender[*edge_index]) {
            Some(cap) => std::cmp::min(enter, cap),
            None => enter,
        };
        carry = produce(edge, &state.tender[*edge_index], &take);
        takes.push(take);
    }
    Some(Sim {
        takes,
        delivered: carry,
    })
}

fn push(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    source: &BigInt,
    room: Option<&BigInt>,
) -> Option<Step> {
    let sim = simulate(problem, state, route_index, source)?;
    if sim.takes.first() != Some(source) || sim.delivered.is_zero() {
        return None;
    }
    let route = &problem.routes[route_index];
    let gas = step_gas(problem, state, route, &sim.takes);
    if let Some(room) = room {
        if &gas > room {
            return None;
        }
    }
    Some(stamp(sim.takes, sim.delivered, gas))
}

fn stamp(takes: Vec<BigInt>, delivered: BigInt, gas: BigInt) -> Step {
    let free = takes.first().is_none_or(Zero::is_zero);
    let den = if free {
        BigInt::from(1)
    } else {
        takes[0].clone()
    };
    Step {
        takes,
        delivered: delivered.clone(),
        gas,
        eff: Eff {
            free,
            num: delivered,
            den,
        },
    }
}

fn remaining_input(edge: &Edge, tender: &BigInt) -> Option<BigInt> {
    match &edge.curve {
        Curve::Unlimited => None,
        Curve::Chords(segments) => {
            let total: BigInt = segments.iter().map(|segment| &segment.width).sum();
            if tender >= &total {
                Some(BigInt::from(0))
            } else {
                Some(&total - tender)
            }
        }
    }
}

fn gcd(mut left: BigInt, mut right: BigInt) -> BigInt {
    while !right.is_zero() {
        let rem = &left % &right;
        left = right;
        right = rem;
    }
    left
}

fn try_ask(
    problem: &Problem,
    state: &Live,
    route_index: usize,
    ask: &BigInt,
    room: Option<&BigInt>,
) -> Option<Step> {
    if ask <= &BigInt::from(0) {
        return None;
    }
    let route = &problem.routes[route_index];
    let mut takes = vec![BigInt::from(0); route.edges.len()];
    let mut need = ask.clone();
    for index in (0..route.edges.len()).rev() {
        let edge = &problem.edges[route.edges[index]];
        let is_sink = index + 1 == route.edges.len();
        let cover = if is_sink {
            BigInt::from(0)
        } else {
            std::cmp::min(need.clone(), state.excess(&paid(edge)))
        };
        let produce_need = &need - &cover;
        if produce_need.is_zero() {
            need = BigInt::from(0);
            continue;
        }
        let take = piece_take(edge, &state.tender[route.edges[index]], &produce_need)?;
        takes[index] = take.clone();
        need = take;
    }
    let source = spent(&problem.edges[route.edges[0]]);
    let budget = state
        .budget
        .get(&source)
        .cloned()
        .unwrap_or_else(BigInt::zero);
    if takes[0] > budget {
        return None;
    }
    let gas = step_gas(problem, state, route, &takes);
    if let Some(room) = room {
        if &gas > room {
            return None;
        }
    }
    let last = *route.edges.last()?;
    let delivered = produce(
        &problem.edges[last],
        &state.tender[last],
        &takes[takes.len() - 1],
    );
    if &delivered < ask {
        return None;
    }
    Some(stamp(takes, delivered, gas))
}

fn piece_take(edge: &Edge, tender: &BigInt, need: &BigInt) -> Option<BigInt> {
    least_take(edge, tender, need)
}

fn least_take(edge: &Edge, tender: &BigInt, need: &BigInt) -> Option<BigInt> {
    if need <= &BigInt::from(0) {
        return Some(BigInt::from(0));
    }
    match &edge.curve {
        Curve::Unlimited => Some(need.clone()),
        Curve::Chords(segments) => least_across(segments, tender, need),
    }
}

fn least_across(segments: &[Chord], tender: &BigInt, need: &BigInt) -> Option<BigInt> {
    let mut skip = tender.clone();
    let mut need = need.clone();
    let mut spent = BigInt::from(0);
    for segment in segments {
        if skip >= segment.width {
            skip -= &segment.width;
            continue;
        }
        let offset = skip.clone();
        let rem_w = &segment.width - &offset;
        let paid = (&segment.output * &offset) / &segment.width;
        let rem_out = &segment.output - &paid;
        if rem_out <= BigInt::from(0) {
            spent += &rem_w;
            skip = BigInt::from(0);
            continue;
        }
        if need <= rem_out {
            let head = Head {
                offset,
                rem_w,
                output: segment.output.clone(),
                width: segment.width.clone(),
            };
            return Some(&spent + &least_on_head(&head, &need)?);
        }
        need -= rem_out;
        spent += rem_w;
        skip = BigInt::from(0);
    }
    None
}

struct Head {
    offset: BigInt,
    rem_w: BigInt,
    output: BigInt,
    width: BigInt,
}

fn head(segments: &[Chord], tender: &BigInt) -> Option<Head> {
    let mut left = tender.clone();
    for segment in segments {
        if left >= segment.width {
            left -= &segment.width;
            continue;
        }
        let paid = (&segment.output * &left) / &segment.width;
        let rem_w = &segment.width - &left;
        if paid >= segment.output || rem_w.is_zero() {
            return None;
        }
        return Some(Head {
            offset: left,
            rem_w,
            output: segment.output.clone(),
            width: segment.width.clone(),
        });
    }
    None
}

/// Least input on this slope segment whose original chord meets `need`.
fn least_on_head(head: &Head, need: &BigInt) -> Option<BigInt> {
    if need <= &BigInt::from(0) {
        return Some(BigInt::from(0));
    }
    let paid = (&head.output * &head.offset) / &head.width;
    let target = paid + need;
    if target > head.output {
        return None;
    }
    let end = (&target * &head.width + &head.output - 1) / &head.output;
    if end <= head.offset {
        return None;
    }
    let take = end - &head.offset;
    if take > head.rem_w {
        return None;
    }
    Some(take)
}

fn produce(edge: &Edge, tender: &BigInt, take: &BigInt) -> BigInt {
    if take.is_zero() {
        return BigInt::from(0);
    }
    let next = tender + take;
    quote_curve(&edge.curve, &next) - quote_curve(&edge.curve, tender)
}

fn step_gas(problem: &Problem, state: &Live, route: &Route, takes: &[BigInt]) -> BigInt {
    let mut gas = BigInt::from(0);
    let mut seen = BTreeSet::new();
    for (index, take) in takes.iter().enumerate() {
        if take.is_zero() {
            continue;
        }
        let edge_index = route.edges[index];
        let edge = &problem.edges[edge_index];
        for (action, _) in paid_shares(edge, &state.tender[edge_index], take) {
            if state.opened.contains(&action) || !seen.insert(action.clone()) {
                continue;
            }
            if let Some(cost) = problem.gas.get(&action) {
                gas += cost;
            }
        }
    }
    gas
}

/// Input of `take` that lands on each action, starting after `tender`.
fn paid_shares(edge: &Edge, tender: &BigInt, take: &BigInt) -> Vec<(Action, BigInt)> {
    if take.sign() != num_bigint::Sign::Plus {
        return Vec::new();
    }
    match &edge.curve {
        Curve::Unlimited => vec![(edge.action.clone(), take.clone())],
        Curve::Chords(chords) => {
            let mut shares: Vec<(Action, BigInt)> = Vec::new();
            let mut skip = tender.clone();
            let mut left = take.clone();
            for chord in chords {
                if skip >= chord.width {
                    skip -= &chord.width;
                    continue;
                }
                let room = &chord.width - &skip;
                let used = std::cmp::min(left.clone(), room);
                if !used.is_zero() {
                    if let Some((_, amount)) = shares
                        .iter_mut()
                        .find(|(action, _)| action == &chord.action)
                    {
                        *amount += &used;
                    } else {
                        shares.push((chord.action.clone(), used.clone()));
                    }
                }
                left -= &used;
                skip = BigInt::from(0);
                if left.is_zero() {
                    break;
                }
            }
            shares
        }
    }
}

fn tenders(problem: &Problem, edge_tender: &[BigInt]) -> Vec<Tender> {
    let mut acc: BTreeMap<(Action, Token, Token), BigInt> = BTreeMap::new();
    for (index, tender) in edge_tender.iter().enumerate() {
        if tender.is_zero() {
            continue;
        }
        let edge = &problem.edges[index];
        let Some((token_in, token_out)) = edge.token_in().zip(edge.token_out()) else {
            continue;
        };
        for (action, input) in paid_shares(edge, &BigInt::from(0), tender) {
            *acc.entry((action, token_in, token_out)).or_default() += input;
        }
    }
    acc.into_iter()
        .map(|((action, token_in, token_out), input)| Tender {
            action,
            token_in,
            token_out,
            input,
        })
        .collect()
}

fn route_touches(route: &Route, problem: &Problem, hubs: &HashSet<Token>) -> bool {
    route.edges.iter().any(|index| {
        let edge = &problem.edges[*index];
        edge.token_in().is_some_and(|token| hubs.contains(&token))
            || edge.token_out().is_some_and(|token| hubs.contains(&token))
    })
}

fn source(problem: &Problem, route: &Route) -> Option<Token> {
    let index = *route.edges.first()?;
    problem.edges.get(index)?.token_in()
}

fn sink(problem: &Problem, route: &Route) -> Option<Token> {
    let index = *route.edges.last()?;
    problem.edges.get(index)?.token_out()
}

fn forward(problem: &Problem, route: &Route, budget: &BigInt) -> BigInt {
    let mut flow = budget.clone();
    for index in &route.edges {
        let Some(edge) = problem.edges.get(*index) else {
            return BigInt::from(0);
        };
        flow = quote_curve(&edge.curve, &flow);
    }
    flow
}

fn spent(edge: &Edge) -> Token {
    edge.token_in().expect("a checked edge spends one token")
}

fn paid(edge: &Edge) -> Token {
    edge.token_out().expect("a checked edge pays one token")
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn chord_edge(width: i64, output: i64) -> Edge {
        let contract = action::Address::new([1; 20]).expect("contract");
        let swap = action::Swap::new(
            contract,
            contract.pool_id(),
            Token([0; 20]),
            Token([1; 20]),
            action::Invariant::Product,
        )
        .expect("swap");
        let action = Action::Swap(swap);
        Edge {
            action: action.clone(),
            curve: Curve::Chords(vec![Chord {
                width: BigInt::from(width),
                output: BigInt::from(output),
                action,
            }]),
        }
    }

    #[test]
    fn the_residual_inverse_meets_the_original_pay() {
        for width in 1..8 {
            for output in 1..8 {
                for offset in 0..width {
                    let edge = chord_edge(width, output);
                    let paid =
                        (&BigInt::from(output) * &BigInt::from(offset)) / &BigInt::from(width);
                    let remaining = BigInt::from(output) - paid;
                    let mut need = BigInt::from(1);
                    while need <= remaining {
                        let take = piece_take(&edge, &BigInt::from(offset), &need)
                            .expect("the head can fund the need");
                        let paid = produce(&edge, &BigInt::from(offset), &take);
                        assert!(paid >= need);
                        if take > BigInt::from(1) {
                            let smaller = produce(&edge, &BigInt::from(offset), &(&take - 1));
                            assert!(smaller < need);
                        }
                        need += 1;
                    }
                }
            }
        }
    }

    fn hop(input: u8, output: u8, pool: u8, chords: &[(i64, i64)]) -> Edge {
        let contract = action::Address::new([pool; 20]).expect("contract");
        let swap = action::Swap::new(
            contract,
            contract.pool_id(),
            Token([input; 20]),
            Token([output; 20]),
            action::Invariant::Product,
        )
        .expect("swap");
        let action = Action::Swap(swap);
        Edge {
            action: action.clone(),
            curve: Curve::Chords(
                chords
                    .iter()
                    .map(|(width, output)| Chord {
                        width: BigInt::from(*width),
                        output: BigInt::from(*output),
                        action: action.clone(),
                    })
                    .collect(),
            ),
        }
    }

    fn two_hop(second: &[(i64, i64)]) -> Problem {
        Problem {
            inputs: vec![crate::Input {
                token: Token([1; 20]),
                amount: BigInt::from(100),
            }],
            outputs: vec![crate::Output {
                token: Token([3; 20]),
                weight: BigInt::from(1),
            }],
            edges: vec![hop(1, 2, 1, &[(8, 13), (8, 5)]), hop(2, 3, 2, second)],
            routes: vec![Route { edges: vec![0, 1] }],
            hubs: Vec::new(),
            gas: BTreeMap::new(),
            gas_ceiling: None,
        }
    }

    fn scanned_span(problem: &Problem, state: &Live, limit: &BigInt) -> Option<BigInt> {
        let one = BigInt::from(1);
        if !stays_on_heads(problem, state, 0, &one) {
            return None;
        }
        let mut best = one.clone();
        let mut take = BigInt::from(2);
        while &take <= limit {
            if !stays_on_heads(problem, state, 0, &take) {
                break;
            }
            best = take.clone();
            take += 1;
        }
        Some(best)
    }

    #[test]
    fn the_span_matches_the_head_boundary() {
        for second in [
            &[(10, 9), (10, 4)][..],
            &[(4, 5), (10, 3)][..],
            &[(6, 6)][..],
        ] {
            let problem = two_hop(second);
            let mut state = Live::open(&problem, &BigInt::from(1));
            for limit in 1..36 {
                let limit = BigInt::from(limit);
                assert_eq!(
                    span_take(&problem, &state, 0, &limit),
                    scanned_span(&problem, &state, &limit),
                    "limit {limit}"
                );
            }
            state.tender[0] = BigInt::from(3);
            state.tender[1] = BigInt::from(2);
            for limit in 1..36 {
                let limit = BigInt::from(limit);
                assert_eq!(
                    span_take(&problem, &state, 0, &limit),
                    scanned_span(&problem, &state, &limit),
                    "tendered {limit}"
                );
            }
        }
    }
}
