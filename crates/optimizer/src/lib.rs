//! Basket allocation over actions.
//!
//! A problem names input tokens with amounts, output tokens with positive
//! weights, and the routes that connect them. Each edge is one [`action::Action`]
//! and the chords of that action. A custodial wrap has no chords: it pays one
//! token out for one token in, at any size. A route is a sequence of edges.
//!
//! [`optimize`] bisects the basket count. A count `Q` asks each output for
//! `Q` times its weight. The search walks routes from the outputs back to
//! the inputs and books one net input on each edge. Two routes that share an
//! edge book one tender, and the quote is [`routing::quote`] on that net.
//! The count is the largest one whose inputs fit the amounts.
//!
//! Gas is the sum of the gas units of the actions that receive input. Two
//! edges that name the same action keep two quotes, and the gas is charged
//! once. A larger count wins. Equal counts keep the smaller gas: once the
//! count is fixed, a fill that opens less gas replaces the plan when it still
//! meets the count. A gas ceiling drops a candidate that would pass it. The
//! allocator does not read a venue kind.

mod solve;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashSet};
use std::fmt;

use action::Action;
use num_bigint::BigInt;
use num_traits::Zero;

pub use action::Token;

/// An input token and the amount the basket may spend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Input {
    pub token: Token,
    pub amount: BigInt,
}

/// An output token and its weight in the basket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub token: Token,
    pub weight: BigInt,
}

/// The quote of one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Curve {
    /// One token out per token in, at any size. A custodial wrap or unwrap.
    Unlimited,
    /// Chords of this action, steepest first.
    Chords(Vec<Chord>),
}

/// One constant marginal of an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chord {
    pub width: BigInt,
    pub output: BigInt,
}

/// One action a route can take. Routes share an edge by index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub action: Action,
    pub curve: Curve,
}

impl Edge {
    /// Token the action spends. A wide action has no single spent token.
    #[must_use]
    pub fn token_in(&self) -> Option<Token> {
        pair(&self.action).map(|(token_in, _)| token_in)
    }

    /// Token the action pays. A wide action has no single paid token.
    #[must_use]
    pub fn token_out(&self) -> Option<Token> {
        pair(&self.action).map(|(_, token_out)| token_out)
    }
}

/// Indexes into [`Problem::edges`], from an input token toward an output token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub edges: Vec<usize>,
}

/// The route set, the basket weights, and the gas of opening an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub inputs: Vec<Input>,
    pub outputs: Vec<Output>,
    pub edges: Vec<Edge>,
    pub routes: Vec<Route>,
    /// Tokens that break a tie toward a path that touches one.
    pub hubs: Vec<Token>,
    /// Gas units charged the first time an action receives input.
    pub gas: BTreeMap<Action, BigInt>,
    /// Largest gas the plan may open. `None` leaves it unbounded.
    pub gas_ceiling: Option<BigInt>,
}

/// Flow booked on one route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flow {
    pub route: usize,
    /// Input spent at the source of the route.
    pub input: BigInt,
    /// Output delivered at the sink of the route.
    pub output: BigInt,
}

/// Net input of one action on one ordered pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tender {
    pub action: Action,
    pub token_in: Token,
    pub token_out: Token,
    pub input: BigInt,
}

/// The basket the inputs can fund, and the flows that fund it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub count: BigInt,
    pub flows: Vec<Flow>,
    pub spent: Vec<(Token, BigInt)>,
    pub received: Vec<(Token, BigInt)>,
    pub tenders: Vec<Tender>,
    pub gas: BigInt,
}

/// A problem the allocator will not run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptimizeError {
    NoInput,
    NoOutput,
    ZeroAmount,
    ZeroWeight,
    DuplicateInput,
    DuplicateOutput,
    Overlap,
    NegativeGas,
    Wide,
    RouteEdge,
    Disconnected,
    Cycle,
    SecondInput,
    BrokenCurve,
}

impl fmt::Display for OptimizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            OptimizeError::NoInput => "the trade spends nothing",
            OptimizeError::NoOutput => "the trade demands nothing",
            OptimizeError::ZeroAmount => "an input amount is not positive",
            OptimizeError::ZeroWeight => "an output weight is not positive",
            OptimizeError::DuplicateInput => "an input token is repeated",
            OptimizeError::DuplicateOutput => "an output token is repeated",
            OptimizeError::Overlap => "a token is both spent and demanded",
            OptimizeError::NegativeGas => "a gas amount is negative",
            OptimizeError::Wide => "an action spends or pays more than one token",
            OptimizeError::RouteEdge => "a route names an edge it does not have",
            OptimizeError::Disconnected => "a route does not connect an input to an output",
            OptimizeError::Cycle => "a route repeats a token",
            OptimizeError::SecondInput => "a route enters an input token",
            OptimizeError::BrokenCurve => {
                "a curve is empty, falls, rises, or disagrees with the action"
            }
        };
        f.write_str(text)
    }
}

impl std::error::Error for OptimizeError {}

/// Largest basket the routes can fund inside the output weights.
pub fn optimize(problem: &Problem) -> Result<Plan, OptimizeError> {
    check(problem)?;
    let bound = solve::output_bound(problem);
    if bound.is_zero() {
        return Ok(zero_plan(problem));
    }
    let mut lo = BigInt::from(0);
    let mut hi = &bound + 1;
    let mut best = zero_plan(problem);
    while &lo + 1 < hi {
        let mid = (&lo + &hi) / 2;
        if let Some(plan) = solve::meet(problem, &mid) {
            lo = plan.count.clone();
            if lo >= hi {
                hi = &lo + 1;
            }
            best = plan;
        } else {
            hi = mid;
        }
    }
    Ok(solve::polish(problem, best))
}

fn zero_plan(problem: &Problem) -> Plan {
    Plan {
        count: BigInt::from(0),
        flows: Vec::new(),
        spent: problem
            .inputs
            .iter()
            .map(|input| (input.token, BigInt::from(0)))
            .collect(),
        received: problem
            .outputs
            .iter()
            .map(|output| (output.token, BigInt::from(0)))
            .collect(),
        tenders: Vec::new(),
        gas: BigInt::from(0),
    }
}

fn check(problem: &Problem) -> Result<(), OptimizeError> {
    if problem.inputs.is_empty() {
        return Err(OptimizeError::NoInput);
    }
    if problem.outputs.is_empty() {
        return Err(OptimizeError::NoOutput);
    }
    let mut inputs = HashSet::new();
    for input in &problem.inputs {
        if input.amount <= BigInt::from(0) {
            return Err(OptimizeError::ZeroAmount);
        }
        if !inputs.insert(input.token) {
            return Err(OptimizeError::DuplicateInput);
        }
    }
    let mut outputs = HashSet::new();
    for output in &problem.outputs {
        if output.weight <= BigInt::from(0) {
            return Err(OptimizeError::ZeroWeight);
        }
        if !outputs.insert(output.token) {
            return Err(OptimizeError::DuplicateOutput);
        }
        if inputs.contains(&output.token) {
            return Err(OptimizeError::Overlap);
        }
    }
    if problem.gas.values().any(|gas| gas < &BigInt::from(0)) {
        return Err(OptimizeError::NegativeGas);
    }
    if let Some(ceiling) = &problem.gas_ceiling {
        if ceiling < &BigInt::from(0) {
            return Err(OptimizeError::NegativeGas);
        }
    }
    for edge in &problem.edges {
        if pair(&edge.action).is_none() {
            return Err(OptimizeError::Wide);
        }
        match &edge.curve {
            Curve::Unlimited => {
                if !custodial(&edge.action) {
                    return Err(OptimizeError::BrokenCurve);
                }
            }
            Curve::Chords(chords) => {
                if custodial(&edge.action) || !curve_rises_nowhere(chords) {
                    return Err(OptimizeError::BrokenCurve);
                }
            }
        }
    }
    for route in &problem.routes {
        check_route(problem, route, &inputs, &outputs)?;
    }
    Ok(())
}

fn curve_rises_nowhere(chords: &[Chord]) -> bool {
    if chords.is_empty() {
        return false;
    }
    let mut previous: Option<&Chord> = None;
    for chord in chords {
        if chord.width <= BigInt::from(0) || chord.output <= BigInt::from(0) {
            return false;
        }
        if let Some(previous) = previous {
            if &chord.output * &previous.width > &previous.output * &chord.width {
                return false;
            }
        }
        previous = Some(chord);
    }
    true
}

fn custodial(action: &Action) -> bool {
    matches!(action, Action::Wrap(_) | Action::Unwrap(_))
}

/// The spent token and the paid token when the action names one of each.
fn pair(action: &Action) -> Option<(Token, Token)> {
    match action {
        Action::Wrap(inner) => Some((inner.underlying, inner.wrapper)),
        Action::Unwrap(inner) => Some((inner.wrapper, inner.underlying)),
        Action::RateWrap(inner) => Some((inner.underlying, inner.wrapper)),
        Action::RateUnwrap(inner) => Some((inner.wrapper, inner.underlying)),
        Action::Swap(inner) => Some((inner.token_in, inner.token_out)),
        Action::MintLiquidity(inner) if inner.tokens.len() == 1 => {
            Some((inner.tokens[0], inner.share))
        }
        Action::BurnLiquidity(inner) if inner.tokens.len() == 1 => {
            Some((inner.share, inner.tokens[0]))
        }
        Action::DepositVault(inner) => Some((inner.asset, inner.vault)),
        Action::RedeemVault(inner) => Some((inner.vault, inner.asset)),
        Action::Supply(inner) => Some((inner.asset, inner.receipt)),
        Action::Withdraw(inner) => Some((inner.receipt, inner.asset)),
        Action::Stake(inner) => Some((inner.asset, inner.receipt)),
        Action::Unstake(inner) => Some((inner.receipt, inner.asset)),
        Action::Convert(inner) => Some((inner.token_in, inner.token_out)),
        Action::Fill(inner) if inner.spend.len() == 1 && inner.pay.len() == 1 => {
            Some((inner.spend[0], inner.pay[0]))
        }
        Action::Split(_)
        | Action::Merge(_)
        | Action::MintLiquidity(_)
        | Action::BurnLiquidity(_)
        | Action::Fill(_) => None,
    }
}

pub(crate) fn quote_curve(curve: &Curve, input: &BigInt) -> BigInt {
    match curve {
        Curve::Unlimited => {
            if input <= &BigInt::from(0) {
                BigInt::from(0)
            } else {
                input.clone()
            }
        }
        Curve::Chords(chords) => {
            let marker = routing::Action {
                kind: 0,
                pool: [0; 32],
            };
            let segments: Vec<routing::SlopeSegment> = chords
                .iter()
                .map(|chord| routing::SlopeSegment {
                    width: chord.width.clone(),
                    output: chord.output.clone(),
                    action: marker,
                })
                .collect();
            routing::quote(&segments, input)
        }
    }
}

fn check_route(
    problem: &Problem,
    route: &Route,
    inputs: &HashSet<Token>,
    outputs: &HashSet<Token>,
) -> Result<(), OptimizeError> {
    if route.edges.is_empty() {
        return Err(OptimizeError::Disconnected);
    }
    let mut seen = HashSet::new();
    let mut previous: Option<Token> = None;
    for (index, edge_index) in route.edges.iter().enumerate() {
        let Some(edge) = problem.edges.get(*edge_index) else {
            return Err(OptimizeError::RouteEdge);
        };
        let (Some(token_in), Some(token_out)) = (edge.token_in(), edge.token_out()) else {
            return Err(OptimizeError::Wide);
        };
        if index == 0 {
            if !inputs.contains(&token_in) {
                return Err(OptimizeError::Disconnected);
            }
        } else if previous != Some(token_in) {
            return Err(OptimizeError::Disconnected);
        }
        if token_in == token_out || !seen.insert(token_in) {
            return Err(OptimizeError::Cycle);
        }
        if seen.contains(&token_out) {
            return Err(OptimizeError::Cycle);
        }
        if index > 0 && inputs.contains(&token_in) || inputs.contains(&token_out) {
            return Err(OptimizeError::SecondInput);
        }
        previous = Some(token_out);
    }
    let Some(sink) = previous else {
        return Err(OptimizeError::Disconnected);
    };
    if !seen.insert(sink) {
        return Err(OptimizeError::Cycle);
    }
    if !outputs.contains(&sink) {
        return Err(OptimizeError::Disconnected);
    }
    Ok(())
}
