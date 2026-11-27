//! Lockstep fill of the basket.
//!
//! The outputs are the inputs of the terminal edge. Each slice raises every
//! output that is still at the minimum pace, on that output's steepest
//! residual route. Downstream takes sum onto a shared edge, and that edge
//! takes one inverse.

use std::collections::{HashMap, HashSet};

use action::Action;
use num_bigint::BigInt;
use num_traits::{One, Zero};

use crate::solve::{self, ChordRoom};
use crate::{Edge, Flow, Plan, Problem, Route, Token};

pub(crate) fn fund(problem: &Problem, cap: Option<&BigInt>) -> Plan {
    let ceiling = match cap {
        Some(cap) => cap.clone(),
        None => {
            let bound = solve::output_bound(problem);
            if bound.is_zero() {
                return State::open(problem).plan(problem);
            }
            bound
        }
    };
    let mut state = State::open(problem);
    while state.pace(problem) < ceiling && state.step(problem, &ceiling) {}
    state.plan(problem)
}

struct State {
    inputs: HashSet<Token>,
    budget: HashMap<Token, BigInt>,
    held: HashMap<Token, BigInt>,
    tender: Vec<BigInt>,
    route_in: Vec<BigInt>,
    route_out: Vec<BigInt>,
    opened: HashSet<Action>,
    gas: BigInt,
}

struct Choice {
    output: usize,
    route: usize,
    need: BigInt,
    /// Output this route can still pay on its open chords.
    sink: BigInt,
}

struct Quote {
    takes: HashMap<usize, BigInt>,
    order: Vec<usize>,
    spend: HashMap<Token, BigInt>,
    sink_out: HashMap<usize, BigInt>,
    gas: BigInt,
}

struct Draft {
    takes: HashMap<usize, BigInt>,
    order: Vec<usize>,
    spend: HashMap<Token, BigInt>,
    demands: Vec<(usize, BigInt)>,
}

struct Rank {
    route: usize,
    source_in: BigInt,
    slope_num: BigInt,
    slope_den: BigInt,
    budget: BigInt,
    hub: bool,
}

impl State {
    fn open(problem: &Problem) -> Self {
        Self {
            inputs: problem.inputs.iter().map(|input| input.token).collect(),
            budget: problem
                .inputs
                .iter()
                .map(|input| (input.token, input.amount.clone()))
                .collect(),
            held: HashMap::new(),
            tender: vec![BigInt::from(0); problem.edges.len()],
            route_in: vec![BigInt::from(0); problem.routes.len()],
            route_out: vec![BigInt::from(0); problem.routes.len()],
            opened: HashSet::new(),
            gas: BigInt::from(0),
        }
    }

    fn pace(&self, problem: &Problem) -> BigInt {
        problem
            .outputs
            .iter()
            .map(|output| {
                self.held
                    .get(&output.token)
                    .cloned()
                    .unwrap_or_else(BigInt::zero)
                    / &output.weight
            })
            .min()
            .unwrap_or_else(BigInt::zero)
    }

    fn step(&mut self, problem: &Problem, ceiling: &BigInt) -> bool {
        let pace = self.pace(problem);
        let needs = self.unit_needs(problem, &pace);
        if needs.is_empty() {
            return false;
        }
        let mut banned: HashMap<usize, HashSet<usize>> = HashMap::new();
        loop {
            let Some(choices) = self.select(problem, &needs, &banned) else {
                return false;
            };
            let hi = self.slice_hi(problem, ceiling, &pace);
            if hi >= BigInt::one() {
                if let Some(quote) = self.largest(problem, &choices, &pace, &hi) {
                    self.apply(problem, &quote);
                    #[cfg(test)]
                    probe::step();
                    return true;
                }
            }
            if let Some(quote) = self.micro(problem, &choices) {
                self.apply(problem, &quote);
                #[cfg(test)]
                probe::step();
                return true;
            }
            let mut banned_one = false;
            for choice in &choices {
                if self.max_need(problem, choice.route, &choice.need).is_none() {
                    banned
                        .entry(choice.output)
                        .or_default()
                        .insert(choice.route);
                    banned_one = true;
                }
            }
            if !banned_one {
                return false;
            }
        }
    }

    fn unit_needs(&self, problem: &Problem, pace: &BigInt) -> Vec<(usize, BigInt)> {
        let mut needs = Vec::new();
        for (index, output) in problem.outputs.iter().enumerate() {
            let held = self
                .held
                .get(&output.token)
                .cloned()
                .unwrap_or_else(BigInt::zero);
            if &held / &output.weight != *pace {
                continue;
            }
            let need = (pace + 1) * &output.weight - &held;
            if need > BigInt::zero() {
                needs.push((index, need));
            }
        }
        needs
    }

    fn slice_hi(&self, problem: &Problem, ceiling: &BigInt, pace: &BigInt) -> BigInt {
        let mut hi = ceiling - pace;
        for output in &problem.outputs {
            let held = self
                .held
                .get(&output.token)
                .cloned()
                .unwrap_or_else(BigInt::zero);
            let floor = &held / &output.weight;
            if floor > *pace {
                let gap = floor - pace;
                if gap < hi {
                    hi = gap;
                }
            }
        }
        hi
    }

    fn select(
        &self,
        problem: &Problem,
        needs: &[(usize, BigInt)],
        banned: &HashMap<usize, HashSet<usize>>,
    ) -> Option<Vec<Choice>> {
        let mut choices = Vec::with_capacity(needs.len());
        for (output, need) in needs {
            let (route, sink) = self.best_route(problem, *output, banned.get(output))?;
            choices.push(Choice {
                output: *output,
                route,
                need: need.clone(),
                sink,
            });
        }
        Some(choices)
    }

    fn best_route(
        &self,
        problem: &Problem,
        output: usize,
        banned: Option<&HashSet<usize>>,
    ) -> Option<(usize, BigInt)> {
        let token = problem.outputs[output].token;
        let mut best: Option<Rank> = None;
        for (index, route) in problem.routes.iter().enumerate() {
            if banned.is_some_and(|banned| banned.contains(&index)) {
                continue;
            }
            if sink_token(problem, route) != Some(token) {
                continue;
            }
            let Some(rank) = self.rank(problem, index) else {
                continue;
            };
            if best.as_ref().is_none_or(|current| better(&rank, current)) {
                best = Some(rank);
            }
        }
        best.map(|rank| (rank.route, rank.slope_num))
    }

    fn rank(&self, problem: &Problem, route: usize) -> Option<Rank> {
        let quote = self.piece(problem, route)?;
        let sink_out = quote.sink_out.get(&route)?.clone();
        if sink_out.is_zero() {
            return None;
        }
        let source_in = quote
            .spend
            .values()
            .fold(BigInt::zero(), |sum, spend| sum + spend);
        let slope_den = if source_in.is_zero() {
            BigInt::one()
        } else {
            source_in.clone()
        };
        Some(Rank {
            route,
            source_in,
            slope_num: sink_out,
            slope_den,
            budget: self.route_budget(problem, route),
            hub: touches_hub(problem, route),
        })
    }

    /// Largest sink this route can pay while every take stays on its open chord.
    ///
    /// The residual output is pushed forward. The source spend is the least
    /// input that pays that sink.
    fn piece(&self, problem: &Problem, route: usize) -> Option<Quote> {
        let edges = &problem.routes.get(route)?.edges;
        if edges.is_empty() {
            return None;
        }
        let mut produced = BigInt::zero();
        for (index, edge_index) in edges.iter().enumerate() {
            let edge = problem.edges.get(*edge_index)?;
            let token_in = token_in(edge)?;
            let mut available = if index == 0 {
                self.budget
                    .get(&token_in)
                    .cloned()
                    .unwrap_or_else(BigInt::zero)
            } else {
                produced.clone()
            };
            if index > 0 && !self.inputs.contains(&token_in) {
                if let Some(held) = self.held.get(&token_in) {
                    available += held;
                }
            }
            if available.is_zero() {
                produced = BigInt::zero();
                continue;
            }
            let tender = &self.tender[*edge_index];
            let take = match solve::chord_room(edge, tender) {
                ChordRoom::Unlimited => available,
                ChordRoom::Exhausted => {
                    produced = BigInt::zero();
                    continue;
                }
                ChordRoom::Open { rem_w } => {
                    if available < rem_w {
                        available
                    } else {
                        rem_w
                    }
                }
            };
            if take.is_zero() {
                produced = BigInt::zero();
                continue;
            }
            produced = solve::produce(edge, tender, &take);
        }
        if produced.is_zero() {
            return None;
        }
        let sink_out = produced;
        let mut need = sink_out.clone();
        for (index, edge_index) in edges.iter().enumerate().rev() {
            if need.is_zero() {
                return Some(Self::slope_quote(route, sink_out, HashMap::new()));
            }
            let edge = problem.edges.get(*edge_index)?;
            let tender = &self.tender[*edge_index];
            let take = solve::least_take(edge, tender, &need)?;
            if spills(edge, tender, &take) {
                return None;
            }
            if index == 0 {
                let token_in = token_in(edge)?;
                let mut spend = HashMap::new();
                if self.inputs.contains(&token_in) && !take.is_zero() {
                    let room = self
                        .budget
                        .get(&token_in)
                        .cloned()
                        .unwrap_or_else(BigInt::zero);
                    if take > room {
                        return None;
                    }
                    spend.insert(token_in, take);
                }
                return Some(Self::slope_quote(route, sink_out, spend));
            }
            let token_in = token_in(edge)?;
            let cover = if self.inputs.contains(&token_in) {
                BigInt::zero()
            } else {
                let have = self
                    .held
                    .get(&token_in)
                    .cloned()
                    .unwrap_or_else(BigInt::zero);
                if have < take {
                    have
                } else {
                    take.clone()
                }
            };
            need = &take - &cover;
        }
        None
    }

    fn slope_quote(route: usize, sink_out: BigInt, spend: HashMap<Token, BigInt>) -> Quote {
        let mut sink = HashMap::new();
        sink.insert(route, sink_out);
        Quote {
            takes: HashMap::new(),
            order: Vec::new(),
            spend,
            sink_out: sink,
            gas: BigInt::zero(),
        }
    }

    fn route_budget(&self, problem: &Problem, route: usize) -> BigInt {
        let Some(token) = source_token(problem, &problem.routes[route]) else {
            return BigInt::zero();
        };
        self.budget
            .get(&token)
            .cloned()
            .unwrap_or_else(BigInt::zero)
    }

    fn largest(
        &self,
        problem: &Problem,
        choices: &[Choice],
        pace: &BigInt,
        hi: &BigInt,
    ) -> Option<Quote> {
        let hi = self.piece_hi(problem, choices, pace, hi);
        if hi < BigInt::one() {
            return None;
        }
        let mut lo = BigInt::zero();
        let mut right = &hi + 1;
        let mut proven: Option<BigInt> = None;
        while &lo + 1 < right {
            let mid: BigInt = (&lo + &right) / 2;
            let demands = self.demands_at(problem, choices, pace, &mid);
            if self.feasible(problem, &demands, true) {
                proven = Some(mid.clone());
                lo = mid;
            } else {
                right = mid;
            }
        }
        let delta = proven?;
        self.price(
            problem,
            &self.demands_at(problem, choices, pace, &delta),
            true,
        )
    }

    /// Largest pace the chosen routes can add without leaving their open chords.
    fn piece_hi(
        &self,
        problem: &Problem,
        choices: &[Choice],
        pace: &BigInt,
        hi: &BigInt,
    ) -> BigInt {
        let mut cap = hi.clone();
        for choice in choices {
            let output = &problem.outputs[choice.output];
            let held = self
                .held
                .get(&output.token)
                .cloned()
                .unwrap_or_else(BigInt::zero);
            let room = (&held + &choice.sink) / &output.weight;
            if &room <= pace {
                return BigInt::zero();
            }
            let delta = &room - pace;
            if delta < cap {
                cap = delta;
            }
        }
        cap
    }

    fn demands_at(
        &self,
        problem: &Problem,
        choices: &[Choice],
        pace: &BigInt,
        delta: &BigInt,
    ) -> Vec<(usize, BigInt)> {
        choices
            .iter()
            .map(|choice| {
                let output = &problem.outputs[choice.output];
                let held = self
                    .held
                    .get(&output.token)
                    .cloned()
                    .unwrap_or_else(BigInt::zero);
                let need = (pace + delta) * &output.weight - held;
                (choice.route, need)
            })
            .collect()
    }

    fn micro(&self, problem: &Problem, choices: &[Choice]) -> Option<Quote> {
        let span = choices
            .iter()
            .map(|choice| choice.need.clone())
            .max()
            .unwrap_or_else(BigInt::zero);
        if span <= BigInt::one() {
            return None;
        }
        let mut lo = BigInt::zero();
        let mut right = span.clone();
        let mut best = None;
        while &lo + 1 < right {
            let mid: BigInt = (&lo + &right) / 2;
            let demands: Vec<(usize, BigInt)> = choices
                .iter()
                .map(|choice| {
                    let need: BigInt = &choice.need * &mid / &span;
                    (choice.route, need)
                })
                .filter(|(_, need)| !need.is_zero())
                .collect();
            if demands.len() == choices.len() && self.feasible(problem, &demands, true) {
                best = Some(mid.clone());
                lo = mid;
                continue;
            }
            right = mid;
        }
        let found = best?;
        let demands: Vec<(usize, BigInt)> = choices
            .iter()
            .map(|choice| {
                let need: BigInt = &choice.need * &found / &span;
                (choice.route, need)
            })
            .filter(|(_, need)| !need.is_zero())
            .collect();
        self.price(problem, &demands, true)
    }

    fn max_need(&self, problem: &Problem, route: usize, need: &BigInt) -> Option<Quote> {
        if need.is_zero() {
            return None;
        }
        if self.feasible(problem, &[(route, need.clone())], true) {
            return self.price(problem, &[(route, need.clone())], true);
        }
        let mut lo = BigInt::zero();
        let mut right = need.clone();
        let mut proven: Option<BigInt> = None;
        while &lo + 1 < right {
            let mid: BigInt = (&lo + &right) / 2;
            if self.feasible(problem, &[(route, mid.clone())], true) {
                proven = Some(mid.clone());
                lo = mid;
            } else {
                right = mid;
            }
        }
        let found = proven?;
        self.price(problem, &[(route, found)], true)
    }

    fn feasible(&self, problem: &Problem, demands: &[(usize, BigInt)], chord_cap: bool) -> bool {
        let Some(draft) = self.draft(problem, demands, chord_cap) else {
            return false;
        };
        if problem.gas_ceiling.is_some() {
            self.quote_gas(problem, &draft.takes).is_some()
        } else {
            true
        }
    }

    fn price(
        &self,
        problem: &Problem,
        demands: &[(usize, BigInt)],
        chord_cap: bool,
    ) -> Option<Quote> {
        let draft = self.draft(problem, demands, chord_cap)?;
        let gas = self.quote_gas(problem, &draft.takes)?;
        let mut sink_out = HashMap::new();
        for (route_index, _) in &draft.demands {
            let last = *problem.routes[*route_index].edges.last().unwrap();
            let take = draft.takes.get(&last).cloned().unwrap_or_else(BigInt::zero);
            let gained = solve::produce(&problem.edges[last], &self.tender[last], &take);
            sink_out.insert(*route_index, gained);
        }
        Some(Quote {
            takes: draft.takes,
            order: draft.order,
            spend: draft.spend,
            sink_out,
            gas,
        })
    }

    fn draft(
        &self,
        problem: &Problem,
        demands: &[(usize, BigInt)],
        chord_cap: bool,
    ) -> Option<Draft> {
        #[cfg(test)]
        probe::price();
        let demands: Vec<(usize, BigInt)> = demands
            .iter()
            .filter(|(_, need)| !need.is_zero())
            .cloned()
            .collect();
        if demands.is_empty() {
            return None;
        }
        let mut successors: HashMap<usize, HashSet<usize>> = HashMap::new();
        let mut predecessor: HashMap<usize, usize> = HashMap::new();
        let mut need: HashMap<usize, BigInt> = HashMap::new();
        for (route_index, sink_need) in &demands {
            let route = problem.routes.get(*route_index)?;
            let last = *route.edges.last()?;
            *need.entry(last).or_default() += sink_need;
            for pair in route.edges.windows(2) {
                successors.entry(pair[0]).or_default().insert(pair[1]);
                predecessor.insert(pair[1], pair[0]);
            }
        }
        let mut pending: HashMap<usize, usize> = successors
            .iter()
            .map(|(edge, next)| (*edge, next.len()))
            .collect();
        let mut queue = Vec::new();
        let mut seen = HashSet::new();
        for (route_index, _) in &demands {
            let last = *problem.routes[*route_index].edges.last().unwrap();
            if seen.insert(last) && !successors.contains_key(&last) {
                queue.push(last);
            }
        }
        let mut contrib: HashMap<usize, BigInt> = HashMap::new();
        let mut held_left = self.held.clone();
        let mut takes: HashMap<usize, BigInt> = HashMap::new();
        let mut order = Vec::new();
        while let Some(edge_index) = queue.pop() {
            let required = need.get(&edge_index).cloned().unwrap_or_else(BigInt::zero);
            let take = if required.is_zero() {
                BigInt::zero()
            } else {
                let edge = &problem.edges[edge_index];
                let tender = &self.tender[edge_index];
                if chord_cap {
                    match solve::open_output(edge, tender) {
                        solve::OpenOut::Unlimited => {}
                        solve::OpenOut::Exhausted => return None,
                        solve::OpenOut::Limited(rem) if required > rem => return None,
                        solve::OpenOut::Limited(_) => {}
                    }
                }
                let take = solve::least_take(edge, tender, &required)?;
                if chord_cap && spills(edge, tender, &take) {
                    return None;
                }
                take
            };
            takes.insert(edge_index, take.clone());
            order.push(edge_index);
            let Some(&upstream) = predecessor.get(&edge_index) else {
                continue;
            };
            *contrib.entry(upstream).or_default() += &take;
            let left = pending.get_mut(&upstream)?;
            *left -= 1;
            if *left == 0 {
                let token = token_out(&problem.edges[upstream])?;
                let sum = contrib.get(&upstream).cloned().unwrap_or_else(BigInt::zero);
                let cover = if self.inputs.contains(&token) {
                    BigInt::zero()
                } else {
                    let have = held_left.get(&token).cloned().unwrap_or_else(BigInt::zero);
                    let cover = if have < sum { have } else { sum.clone() };
                    *held_left.entry(token).or_default() -= &cover;
                    cover
                };
                need.insert(upstream, &sum - &cover);
                queue.push(upstream);
            }
        }
        if takes.values().all(Zero::is_zero) {
            return None;
        }
        let mut spend: HashMap<Token, BigInt> = HashMap::new();
        for (edge_index, take) in &takes {
            if take.is_zero() {
                continue;
            }
            let token = token_in(&problem.edges[*edge_index])?;
            if self.inputs.contains(&token) {
                *spend.entry(token).or_default() += take;
            }
        }
        for (token, spent) in &spend {
            let room = self.budget.get(token).cloned().unwrap_or_else(BigInt::zero);
            if spent > &room {
                return None;
            }
        }
        Some(Draft {
            takes,
            order,
            spend,
            demands,
        })
    }

    fn quote_gas(&self, problem: &Problem, takes: &HashMap<usize, BigInt>) -> Option<BigInt> {
        if problem.gas.is_empty() && problem.gas_ceiling.is_none() {
            return Some(BigInt::zero());
        }
        let mut opened = self.opened.clone();
        let mut gas = BigInt::zero();
        for (edge_index, take) in takes {
            if take.is_zero() {
                continue;
            }
            for (action, _) in
                solve::paid_shares(&problem.edges[*edge_index], &self.tender[*edge_index], take)
            {
                if !opened.insert(action.clone()) {
                    continue;
                }
                if let Some(cost) = problem.gas.get(&action) {
                    gas += cost;
                }
            }
        }
        if let Some(ceiling) = &problem.gas_ceiling {
            if &self.gas + &gas > *ceiling {
                return None;
            }
        }
        Some(gas)
    }

    fn apply(&mut self, problem: &Problem, quote: &Quote) {
        let gas_before = self.gas.clone();
        for edge_index in quote.order.iter().rev() {
            let take = quote
                .takes
                .get(edge_index)
                .cloned()
                .unwrap_or_else(BigInt::zero);
            if take.is_zero() {
                continue;
            }
            let edge = &problem.edges[*edge_index];
            let token_in = token_in(edge).expect("edge spends one token");
            let token_out = token_out(edge).expect("edge pays one token");
            if self.inputs.contains(&token_in) {
                let room = self.budget.get_mut(&token_in).expect("budget");
                *room -= &take;
            } else {
                let held = self.held.entry(token_in).or_default();
                *held -= &take;
            }
            let opened_at = self.tender[*edge_index].clone();
            let gained = solve::produce(edge, &opened_at, &take);
            self.tender[*edge_index] += &take;
            *self.held.entry(token_out).or_default() += &gained;
            for (action, _) in solve::paid_shares(edge, &opened_at, &take) {
                if self.opened.insert(action.clone()) {
                    if let Some(cost) = problem.gas.get(&action) {
                        self.gas += cost;
                    }
                }
            }
        }
        debug_assert_eq!(&self.gas - &gas_before, quote.gas);
        for (route_index, gained) in &quote.sink_out {
            self.route_out[*route_index] += gained;
            let first = problem.routes[*route_index].edges[0];
            let take = quote
                .takes
                .get(&first)
                .cloned()
                .unwrap_or_else(BigInt::zero);
            let owners: Vec<usize> = quote
                .sink_out
                .keys()
                .copied()
                .filter(|other| problem.routes[*other].edges[0] == first)
                .collect();
            if owners.len() <= 1 {
                self.route_in[*route_index] += &take;
            } else {
                let weight: BigInt = owners
                    .iter()
                    .map(|other| {
                        quote
                            .sink_out
                            .get(other)
                            .cloned()
                            .unwrap_or_else(BigInt::zero)
                    })
                    .fold(BigInt::zero(), |sum, gained| sum + gained);
                if weight.is_zero() {
                    continue;
                }
                self.route_in[*route_index] += &take * gained / &weight;
            }
        }
    }

    fn plan(&self, problem: &Problem) -> Plan {
        let received: Vec<(Token, BigInt)> = problem
            .outputs
            .iter()
            .map(|output| {
                (
                    output.token,
                    self.held
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
        let legs: Vec<(BigInt, BigInt)> = problem
            .outputs
            .iter()
            .zip(&received)
            .map(|(output, (_, held))| (held.clone(), output.weight.clone()))
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
            count: routing::basket::basket_count(&legs).unwrap_or_else(BigInt::zero),
            flows,
            spent,
            received,
            tenders: solve::tenders(problem, &self.tender),
            gas: self.gas.clone(),
        }
    }
}

fn better(candidate: &Rank, current: &Rank) -> bool {
    let candidate_free = candidate.source_in.is_zero();
    let current_free = current.source_in.is_zero();
    match (candidate_free, current_free) {
        (true, false) => return true,
        (false, true) => return false,
        (true, true) => {}
        (false, false) => {
            let left = &candidate.slope_num * &current.slope_den;
            let right = &current.slope_num * &candidate.slope_den;
            if left != right {
                return left > right;
            }
        }
    }
    if candidate.budget != current.budget {
        return candidate.budget > current.budget;
    }
    if candidate.hub != current.hub {
        return candidate.hub;
    }
    candidate.route < current.route
}

fn spills(edge: &Edge, tender: &BigInt, take: &BigInt) -> bool {
    match solve::chord_room(edge, tender) {
        ChordRoom::Unlimited => false,
        ChordRoom::Exhausted => !take.is_zero(),
        ChordRoom::Open { rem_w } => take > &rem_w,
    }
}

fn touches_hub(problem: &Problem, route: usize) -> bool {
    let hubs: HashSet<Token> = problem.hubs.iter().copied().collect();
    problem.routes[route].edges.iter().any(|index| {
        let edge = &problem.edges[*index];
        edge.token_in().is_some_and(|token| hubs.contains(&token))
            || edge.token_out().is_some_and(|token| hubs.contains(&token))
    })
}

fn source_token(problem: &Problem, route: &Route) -> Option<Token> {
    let index = *route.edges.first()?;
    problem.edges.get(index)?.token_in()
}

fn sink_token(problem: &Problem, route: &Route) -> Option<Token> {
    let index = *route.edges.last()?;
    problem.edges.get(index)?.token_out()
}

fn token_in(edge: &Edge) -> Option<Token> {
    edge.token_in()
}

fn token_out(edge: &Edge) -> Option<Token> {
    edge.token_out()
}

#[cfg(test)]
pub(crate) mod probe {
    use std::cell::Cell;

    thread_local! {
        static STEPS: Cell<u64> = const { Cell::new(0) };
        static PRICES: Cell<u64> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        STEPS.with(|steps| steps.set(0));
        PRICES.with(|prices| prices.set(0));
    }

    pub(crate) fn steps() -> u64 {
        STEPS.with(Cell::get)
    }

    pub(crate) fn prices() -> u64 {
        PRICES.with(Cell::get)
    }

    pub(crate) fn step() {
        STEPS.with(|steps| steps.set(steps.get() + 1));
    }

    pub(crate) fn price() {
        PRICES.with(|prices| prices.set(prices.get() + 1));
    }
}
