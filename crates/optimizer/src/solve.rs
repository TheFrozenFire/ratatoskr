//! Backward fill of one basket count.
//!
//! A candidate count fixes the output each token must reach. Each step sends
//! the source prefix whose realized sink output per input is highest, and the
//! longest prefix when those rates match. A prefix that would pass the
//! remaining demand is cut to the least input that meets the demand. Output
//! above the next edge's take stays on that token. Two routes that share an
//! edge add to one tender, and the output of that edge is the quote of the sum.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use num_bigint::BigInt;
use num_traits::Zero;
use routing::{quote, Action, SlopeSegment};

use crate::{Curve, Edge, Flow, Plan, Problem, Route, Tender, Token};

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
    meet_ranked(problem, count, Rank::Slope, None)
}

/// Keeps the count and replaces the plan when another fill opens less gas.
pub(crate) fn polish(problem: &Problem, mut best: Plan) -> Plan {
    if best.count.is_zero() || !gas_can_move(problem) {
        return best;
    }
    loop {
        let count = best.count.clone();
        let gas = best.gas.clone();
        if problem.routes.len() <= 10 {
            improve_subsets(problem, &mut best);
        } else {
            improve_pairs(problem, &mut best);
        }
        improve_without(problem, &mut best);
        if let Some(plan) = meet_ranked(problem, &best.count, Rank::Cheap, None) {
            if lex_better(&plan, &best) {
                best = plan;
            }
        }
        if best.count == count && best.gas == gas {
            break;
        }
    }
    best
}

fn meet_ranked(
    problem: &Problem,
    count: &BigInt,
    rank: Rank,
    allow: Option<&[bool]>,
) -> Option<Plan> {
    if count < &BigInt::from(0) {
        return None;
    }
    let mut state = Live::open(problem, count);
    let hubs: HashSet<Token> = problem.hubs.iter().copied().collect();
    loop {
        if state.met() {
            break;
        }
        let room = gas_room(problem, &state.gas);
        let mut open = Vec::new();
        for index in 0..problem.routes.len() {
            if allow.is_some_and(|allow| !allow[index]) {
                continue;
            }
            let sink = sink(problem, &problem.routes[index])?;
            let need = state.remaining(&sink);
            if need.is_zero() {
                continue;
            }
            let Some(step) = offer(problem, &state, index, &need, room.as_ref()) else {
                continue;
            };
            open.push(Candidate {
                index,
                hub: route_touches(&problem.routes[index], problem, &hubs),
                step,
            });
        }
        let mut best: Option<Candidate> = None;
        for candidate in &open {
            if best
                .as_ref()
                .map(|held| candidate.beats(held, rank))
                .unwrap_or(true)
            {
                best = Some(candidate.clone());
            }
        }
        let candidate = best?;
        state.apply(problem, candidate.index, &candidate.step);
    }
    let plan = state.plan(problem);
    if &plan.count < count {
        return None;
    }
    Some(plan)
}

#[derive(Clone, Copy)]
enum Rank {
    Slope,
    Cheap,
}

fn lex_better(plan: &Plan, best: &Plan) -> bool {
    plan.count > best.count || (plan.count == best.count && plan.gas < best.gas)
}

fn gas_can_move(problem: &Problem) -> bool {
    !problem.wrap_gas.is_zero()
        || problem.gas_ceiling.is_some()
        || problem.gas.values().any(|gas| !gas.is_zero())
}

fn improve_subsets(problem: &Problem, best: &mut Plan) {
    let n = problem.routes.len();
    let mut allow = vec![false; n];
    let limit = 1_u32 << n;
    for mask in 1..limit {
        for (index, flag) in allow.iter_mut().enumerate() {
            *flag = (mask & (1_u32 << index)) != 0;
        }
        if !covers(problem, &allow) {
            continue;
        }
        adopt(problem, best, Rank::Slope, Some(&allow));
    }
}

fn improve_pairs(problem: &Problem, best: &mut Plan) {
    let n = problem.routes.len();
    let mut allow = vec![false; n];
    for left in 0..n {
        allow.fill(false);
        allow[left] = true;
        if covers(problem, &allow) {
            adopt(problem, best, Rank::Slope, Some(&allow));
        }
        for right in left + 1..n {
            allow.fill(false);
            allow[left] = true;
            allow[right] = true;
            if covers(problem, &allow) {
                adopt(problem, best, Rank::Slope, Some(&allow));
            }
        }
    }
}

fn improve_without(problem: &Problem, best: &mut Plan) {
    let mut actions: Vec<Action> = best.tenders.iter().map(|tender| tender.action).collect();
    actions.sort();
    actions.dedup();
    for action in actions {
        let muted = mute_action(problem, action);
        adopt(&muted, best, Rank::Slope, None);
        adopt(&muted, best, Rank::Cheap, None);
    }
    for edge in used_wraps(problem, best) {
        let muted = mute_edge(problem, edge);
        adopt(&muted, best, Rank::Slope, None);
        adopt(&muted, best, Rank::Cheap, None);
    }
}

fn adopt(problem: &Problem, best: &mut Plan, rank: Rank, allow: Option<&[bool]>) {
    if let Some(plan) = meet_ranked(problem, &best.count, rank, allow) {
        if lex_better(&plan, best) {
            *best = plan;
        }
    }
}

fn covers(problem: &Problem, allow: &[bool]) -> bool {
    problem.outputs.iter().all(|output| {
        problem
            .routes
            .iter()
            .enumerate()
            .any(|(index, route)| allow[index] && sink(problem, route) == Some(output.token))
    })
}

fn mute_action(problem: &Problem, action: Action) -> Problem {
    let mut muted = problem.clone();
    for edge in &mut muted.edges {
        if let Curve::Conduit(segments) = &mut edge.curve {
            segments.retain(|segment| segment.action != action);
        }
    }
    muted
}

fn mute_edge(problem: &Problem, index: usize) -> Problem {
    let mut muted = problem.clone();
    if let Some(edge) = muted.edges.get_mut(index) {
        edge.curve = Curve::Conduit(Vec::new());
    }
    muted
}

fn used_wraps(problem: &Problem, plan: &Plan) -> Vec<usize> {
    let mut wraps = Vec::new();
    for flow in &plan.flows {
        if flow.input.is_zero() && flow.output.is_zero() {
            continue;
        }
        for index in &problem.routes[flow.route].edges {
            if matches!(problem.edges[*index].curve, Curve::Wrap) {
                wraps.push(*index);
            }
        }
    }
    wraps.sort_unstable();
    wraps.dedup();
    wraps
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
        let source = problem.edges[route.edges[0]].token_in;
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
            if index > 0 {
                let got = self.got.entry(edge.token_in).or_default();
                debug_assert!(*got >= *take);
                *got -= take;
            }
            let old = self.tender[*edge_index].clone();
            self.tender[*edge_index] += take;
            let actual = produce(edge, &old, take);
            *self.got.entry(edge.token_out).or_default() += &actual;
            if let Curve::Conduit(segments) = &edge.curve {
                for action in actions_in_delta(segments, &old, take) {
                    self.opened.insert(action);
                }
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
    fn beats(&self, other: &Self, rank: Rank) -> bool {
        match rank {
            Rank::Slope => self.slope_beats(other),
            Rank::Cheap => self.cheap_beats(other),
        }
    }

    fn slope_beats(&self, other: &Self) -> bool {
        if self.step.eff.free != other.step.eff.free {
            return self.step.eff.free;
        }
        if !self.same_slope(other) {
            return self.steeper_than(other);
        }
        self.tie_beats(other)
    }

    fn cheap_beats(&self, other: &Self) -> bool {
        let group = self.gas_group();
        let other_group = other.gas_group();
        if group != other_group {
            return group < other_group;
        }
        if group == 2 && !self.same_charge(other) {
            return self.cheaper_than(other);
        }
        if !self.same_slope(other) {
            return self.steeper_than(other);
        }
        self.tie_beats(other)
    }

    fn gas_group(&self) -> u8 {
        if self.step.gas.is_zero() {
            if self.step.eff.free {
                0
            } else {
                1
            }
        } else {
            2
        }
    }

    fn same_slope(&self, other: &Self) -> bool {
        &self.step.eff.num * &other.step.eff.den == &other.step.eff.num * &self.step.eff.den
    }

    fn steeper_than(&self, other: &Self) -> bool {
        &self.step.eff.num * &other.step.eff.den > &other.step.eff.num * &self.step.eff.den
    }

    fn same_charge(&self, other: &Self) -> bool {
        &self.step.gas * &other.step.delivered == &other.step.gas * &self.step.delivered
            && self.step.gas == other.step.gas
    }

    fn cheaper_than(&self, other: &Self) -> bool {
        let left = &self.step.gas * &other.step.delivered;
        let right = &other.step.gas * &self.step.delivered;
        left < right || (left == right && self.step.gas < other.step.gas)
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
    let Curve::Conduit(segments) = &problem.edges.get(edge_index)?.curve else {
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
    if let Some((take, gain)) = head_candidate(problem, state, route_index, limit) {
        if ratio_beats(&gain, &take, &best_gain, Some(&best_take)) {
            best_take = take;
            best_gain = gain;
        }
    }
    if let Some(gain) = spent_gain(problem, state, route_index, limit) {
        if ratio_beats(&gain, limit, &best_gain, Some(&best_take)) {
            best_take = limit.clone();
            best_gain = gain;
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
    let Curve::Conduit(segments) = &problem.edges.get(edge_index)?.curve else {
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
        if index > 0 && !state.excess(&edge.token_in).is_zero() {
            return None;
        }
        match &edge.curve {
            Curve::Wrap => {}
            Curve::Conduit(segments) => {
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

fn linear_input(segments: &[SlopeSegment], tender: &BigInt, rate: &BigInt) -> BigInt {
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
            .get(&edge.token_in)
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
            enter += state.excess(&edge.token_in);
        }
        if index == 0 {
            let budget = state
                .budget
                .get(&edge.token_in)
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
        Curve::Wrap => None,
        Curve::Conduit(segments) => {
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
            std::cmp::min(need.clone(), state.excess(&edge.token_out))
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
    let source = problem.edges[route.edges[0]].token_in;
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
        Curve::Wrap => Some(need.clone()),
        Curve::Conduit(segments) => least_across(segments, tender, need),
    }
}

fn least_across(segments: &[SlopeSegment], tender: &BigInt, need: &BigInt) -> Option<BigInt> {
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

fn head(segments: &[SlopeSegment], tender: &BigInt) -> Option<Head> {
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
    match &edge.curve {
        Curve::Wrap => take.clone(),
        Curve::Conduit(segments) => {
            let next = tender + take;
            quote(segments, &next) - quote(segments, tender)
        }
    }
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
        match &edge.curve {
            Curve::Wrap => {
                if state.tender[edge_index].is_zero() {
                    gas += &problem.wrap_gas;
                }
            }
            Curve::Conduit(segments) => {
                for action in actions_in_delta(segments, &state.tender[edge_index], take) {
                    if state.opened.contains(&action) || !seen.insert(action) {
                        continue;
                    }
                    if let Some(cost) = problem.gas.get(&action) {
                        gas += cost;
                    }
                }
            }
        }
    }
    gas
}

fn actions_in_delta(segments: &[SlopeSegment], tender: &BigInt, take: &BigInt) -> Vec<Action> {
    let mut skip = tender.clone();
    let mut left = take.clone();
    let mut actions = Vec::new();
    for segment in segments {
        if left.is_zero() {
            break;
        }
        if skip >= segment.width {
            skip -= &segment.width;
            continue;
        }
        let room = &segment.width - &skip;
        let used = std::cmp::min(room, left.clone());
        if !used.is_zero() {
            actions.push(segment.action);
        }
        left -= used;
        skip = BigInt::from(0);
    }
    actions
}

fn tenders(problem: &Problem, edge_tender: &[BigInt]) -> Vec<Tender> {
    let mut acc: BTreeMap<(Action, Token, Token), BigInt> = BTreeMap::new();
    for (index, tender) in edge_tender.iter().enumerate() {
        if tender.is_zero() {
            continue;
        }
        let edge = &problem.edges[index];
        let Curve::Conduit(segments) = &edge.curve else {
            continue;
        };
        let mut left = tender.clone();
        for segment in segments {
            if left.is_zero() {
                break;
            }
            let take = std::cmp::min(left.clone(), segment.width.clone());
            *acc.entry((segment.action, edge.token_in, edge.token_out))
                .or_default() += &take;
            left -= &take;
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
        hubs.contains(&edge.token_in) || hubs.contains(&edge.token_out)
    })
}

fn source(problem: &Problem, route: &Route) -> Option<Token> {
    let index = *route.edges.first()?;
    Some(problem.edges.get(index)?.token_in)
}

fn sink(problem: &Problem, route: &Route) -> Option<Token> {
    let index = *route.edges.last()?;
    Some(problem.edges.get(index)?.token_out)
}

fn forward(problem: &Problem, route: &Route, budget: &BigInt) -> BigInt {
    let mut flow = budget.clone();
    for index in &route.edges {
        let Some(edge) = problem.edges.get(*index) else {
            return BigInt::from(0);
        };
        flow = match &edge.curve {
            Curve::Wrap => flow,
            Curve::Conduit(segments) => quote(segments, &flow),
        };
    }
    flow
}

#[cfg(test)]
mod tests {
    use super::*;

    fn act(byte: u8) -> Action {
        Action {
            kind: 1,
            pool: [byte; 32],
        }
    }

    #[test]
    fn the_residual_inverse_meets_the_original_pay() {
        for width in 1..8 {
            for output in 1..8 {
                for offset in 0..width {
                    let built = SlopeSegment {
                        width: BigInt::from(width),
                        output: BigInt::from(output),
                        action: act(1),
                    };
                    let edge = Edge {
                        token_in: crate::Token([0; 20]),
                        token_out: crate::Token([1; 20]),
                        curve: Curve::Conduit(vec![built]),
                    };
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
}
