//! Slope books, the forward output ceiling, and the residual inverse.
//!
//! [`hold`] installs one book per chord curve. [`output_bound`] pushes each
//! source budget forward and keeps the tightest output. [`least_take`] is the
//! least input whose quote meets a required output. [`paid_shares`] names the
//! actions that receive a tender.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use action::Action;
use num_bigint::BigInt;
use num_traits::{ToPrimitive, Zero};
use routing::{At, SlopeBook, Spot};

use crate::{quote_curve, Chord, Curve, Edge, Plan, Problem, Route, Tender, Token};

struct Active {
    keys: Vec<usize>,
    books: Vec<SlopeBook>,
}

struct Installed {
    depth: usize,
    active: Active,
}

thread_local! {
    static BOUND: RefCell<Option<Installed>> = const { RefCell::new(None) };
}

struct Bound;

impl Bound {
    fn install(problem: &Problem) -> Self {
        BOUND.with(|slot| {
            let mut slot = slot.borrow_mut();
            if let Some(installed) = slot.as_mut() {
                installed.depth += 1;
            } else {
                *slot = Some(Installed {
                    depth: 1,
                    active: prepare(problem),
                });
            }
        });
        Self
    }
}

fn prepare(problem: &Problem) -> Active {
    let mut keys = Vec::new();
    let mut books = Vec::new();
    for edge in &problem.edges {
        let Curve::Chords(chords) = &edge.curve else {
            continue;
        };
        let Some(book) =
            SlopeBook::prepare(chords.iter().map(|chord| (&chord.width, &chord.output)))
        else {
            continue;
        };
        keys.push(chords.as_ptr() as usize);
        books.push(book);
    }
    Active { keys, books }
}

impl Drop for Bound {
    fn drop(&mut self) {
        BOUND.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(installed) = slot.as_mut() else {
                return;
            };
            installed.depth -= 1;
            if installed.depth == 0 {
                *slot = None;
            }
        });
    }
}

fn with_book<T>(chords: &[Chord], read: impl FnOnce(Option<&SlopeBook>) -> T) -> T {
    BOUND.with(|slot| {
        let active = slot.borrow();
        let book = active.as_ref().and_then(|installed| {
            let key = chords.as_ptr() as usize;
            installed
                .active
                .keys
                .iter()
                .position(|stored| *stored == key)
                .map(|index| &installed.active.books[index])
        });
        read(book)
    })
}

enum Resume {
    At { index: usize, offset: BigInt },
    Past,
    Start,
}

fn resume(chords: &[Chord], tender: &BigInt) -> Resume {
    match with_book(chords, |book| book.map(|book| book.at(tender))) {
        Some(At::Here { index, offset }) => Resume::At { index, offset },
        Some(At::Past) => Resume::Past,
        Some(At::Unsupported) | None => Resume::Start,
    }
}

fn quoted(chords: &[Chord], input: &BigInt) -> Option<BigInt> {
    with_book(chords, |book| {
        book.map(|book| {
            book.quote_u128(input)
                .map(BigInt::from)
                .unwrap_or_else(|| book.quote(input))
        })
    })
}

pub(crate) fn output_bound(problem: &Problem) -> BigInt {
    let _bound = Bound::install(problem);
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

/// Prepare the slope books for every probe this search will run.
pub(crate) fn hold(problem: &Problem) -> impl Drop {
    Bound::install(problem)
}

pub(crate) fn meet(problem: &Problem, count: &BigInt) -> Option<Plan> {
    if count < &BigInt::from(0) {
        return None;
    }
    let _books = hold(problem);
    let plan = crate::fund::fund(problem, Some(count));
    if plan.count >= *count {
        Some(plan)
    } else {
        None
    }
}

fn piece_take(edge: &Edge, tender: &BigInt, need: &BigInt) -> Option<BigInt> {
    least_take(edge, tender, need)
}

pub(crate) fn least_take(edge: &Edge, tender: &BigInt, need: &BigInt) -> Option<BigInt> {
    if need <= &BigInt::from(0) {
        return Some(BigInt::from(0));
    }
    match &edge.curve {
        Curve::Unlimited => Some(need.clone()),
        Curve::Chords(segments) => least_across(segments, tender, need),
    }
}

#[cfg(test)]
thread_local! {
    static CHORD_VISITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_chord_visits() {
    CHORD_VISITS.with(|visits| visits.set(0));
}

#[cfg(test)]
pub(crate) fn chord_visits() -> u64 {
    CHORD_VISITS.with(std::cell::Cell::get)
}

fn least_across(segments: &[Chord], tender: &BigInt, need: &BigInt) -> Option<BigInt> {
    let (start, mut skip) = match resume(segments, tender) {
        Resume::Past => return None,
        Resume::At { index, offset } => (index, offset),
        Resume::Start => (0, tender.clone()),
    };
    let mut need = need.clone();
    let mut spent = BigInt::from(0);
    for segment in segments.iter().skip(start) {
        #[cfg(test)]
        CHORD_VISITS.with(|visits| visits.set(visits.get() + 1));
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

pub(crate) enum ChordRoom {
    Unlimited,
    /// Remaining input width on the open chord.
    Open {
        rem_w: BigInt,
    },
    Exhausted,
}

pub(crate) enum OpenOut {
    Unlimited,
    /// Output the open chord can still pay.
    Limited(BigInt),
    Exhausted,
}

/// Output still available without leaving the open chord.
pub(crate) fn open_output(edge: &Edge, tender: &BigInt) -> OpenOut {
    match &edge.curve {
        Curve::Unlimited => OpenOut::Unlimited,
        Curve::Chords(chords) => match head(chords, tender) {
            None => OpenOut::Exhausted,
            Some(head) => {
                let paid = (&head.output * &head.offset) / &head.width;
                let rem = &head.output - &paid;
                if rem <= BigInt::from(0) {
                    OpenOut::Exhausted
                } else {
                    OpenOut::Limited(rem)
                }
            }
        },
    }
}

pub(crate) fn chord_room(edge: &Edge, tender: &BigInt) -> ChordRoom {
    match &edge.curve {
        Curve::Unlimited => ChordRoom::Unlimited,
        Curve::Chords(chords) => match head(chords, tender) {
            Some(head) => ChordRoom::Open { rem_w: head.rem_w },
            None => ChordRoom::Exhausted,
        },
    }
}

fn head(segments: &[Chord], tender: &BigInt) -> Option<Head> {
    match resume(segments, tender) {
        Resume::Past => return None,
        Resume::At { index, offset } => {
            let segment = &segments[index];
            let paid = (&segment.output * &offset) / &segment.width;
            let rem_w = &segment.width - &offset;
            if paid >= segment.output || rem_w.is_zero() {
                return None;
            }
            return Some(Head {
                offset,
                rem_w,
                output: segment.output.clone(),
                width: segment.width.clone(),
            });
        }
        Resume::Start => {}
    }
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

pub(crate) fn produce(edge: &Edge, tender: &BigInt, take: &BigInt) -> BigInt {
    if take.is_zero() {
        return BigInt::from(0);
    }
    if let Curve::Chords(chords) = &edge.curve {
        if let Some(delta) = with_book(chords, |book| {
            book.and_then(|book| book.delta_u128(tender, take))
                .map(BigInt::from)
        }) {
            return delta;
        }
    }
    let next = tender + take;
    if let Curve::Chords(chords) = &edge.curve {
        if let (Some(after), Some(before)) = (quoted(chords, &next), quoted(chords, tender)) {
            return after - before;
        }
    }
    quote_curve(&edge.curve, &next) - quote_curve(&edge.curve, tender)
}

fn native_shares(
    chords: &[Chord],
    tender: &BigInt,
    take: &BigInt,
) -> Option<Vec<(Action, BigInt)>> {
    let mut left = take.to_u128()?;
    let spot = with_book(chords, |book| book.map(|book| book.spot(tender)))?;
    let (mut index, mut offset) = match spot {
        Spot::Here(seat) => (seat.index, seat.offset),
        Spot::Past => return Some(Vec::new()),
        Spot::Other => return None,
    };
    let mut shares: Vec<(Action, u128)> = Vec::new();
    while left > 0 && index < chords.len() {
        let chord = &chords[index];
        let width = chord.width.to_u128()?;
        if offset >= width {
            offset -= width;
            index += 1;
            continue;
        }
        let used = left.min(width - offset);
        if let Some((_, amount)) = shares
            .iter_mut()
            .find(|(action, _)| action == &chord.action)
        {
            *amount = amount.checked_add(used)?;
        } else {
            shares.push((chord.action.clone(), used));
        }
        left -= used;
        offset = 0;
        index += 1;
    }
    Some(
        shares
            .into_iter()
            .map(|(action, amount)| (action, BigInt::from(amount)))
            .collect(),
    )
}

/// Input of `take` that lands on each action, starting after `tender`.
pub(crate) fn paid_shares(edge: &Edge, tender: &BigInt, take: &BigInt) -> Vec<(Action, BigInt)> {
    if take.sign() != num_bigint::Sign::Plus {
        return Vec::new();
    }
    match &edge.curve {
        Curve::Unlimited => vec![(edge.action.clone(), take.clone())],
        Curve::Chords(chords) => {
            if let Some(shares) = native_shares(chords, tender, take) {
                return shares;
            }
            let (start, mut skip) = match resume(chords, tender) {
                Resume::Past => return Vec::new(),
                Resume::At { index, offset } => (index, offset),
                Resume::Start => (0, tender.clone()),
            };
            let mut shares: Vec<(Action, BigInt)> = Vec::new();
            let mut left = take.clone();
            for chord in chords.iter().skip(start) {
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

pub(crate) fn tenders(problem: &Problem, edge_tender: &[BigInt]) -> Vec<Tender> {
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
        if flow.sign() != num_bigint::Sign::Plus {
            return BigInt::from(0);
        }
        flow = if let Curve::Chords(chords) = &edge.curve {
            with_book(chords, |book| {
                book.and_then(|book| book.delta_u128(&BigInt::from(0), &flow))
                    .map(BigInt::from)
            })
            .unwrap_or_else(|| quote_curve(&edge.curve, &flow))
        } else {
            quote_curve(&edge.curve, &flow)
        };
    }
    flow
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

    fn walked_width(edge: &Edge, tender: &BigInt) -> Option<BigInt> {
        let Curve::Chords(chords) = &edge.curve else {
            return None;
        };
        let mut left = tender.clone();
        for chord in chords {
            if left >= chord.width {
                left -= &chord.width;
                continue;
            }
            let paid = (&chord.output * &left) / &chord.width;
            if paid >= chord.output {
                return None;
            }
            return Some(&chord.width - &left);
        }
        None
    }

    #[test]
    fn the_open_chord_ends_at_the_head_boundary() {
        for second in [
            &[(10, 9), (10, 4)][..],
            &[(4, 5), (10, 3)][..],
            &[(6, 6)][..],
        ] {
            let problem = two_hop(second);
            for edge in &problem.edges {
                for offset in 0..20 {
                    let tender = BigInt::from(offset);
                    let walked = walked_width(edge, &tender);
                    match chord_room(edge, &tender) {
                        ChordRoom::Open { rem_w } => assert_eq!(walked, Some(rem_w)),
                        ChordRoom::Exhausted => assert_eq!(walked, None),
                        ChordRoom::Unlimited => panic!("the hop is chords"),
                    }
                }
            }
        }
    }
}
