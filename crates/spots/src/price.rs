//! Sized round trips through the direct pair, one hub, or two hubs.

use std::collections::{BTreeSet, HashMap, HashSet};

use alloy_primitives::{address, Address};
use num_bigint::BigInt;
use routing::SlopeSegment;
use store_model::Spot;

/// Wrapped ETH. The numeraire for the ether sizes. Not a feed answer.
pub const WETH: Address = address!("0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");

/// USDC. The numeraire for the dollar-token sizes. Not the dollar answer.
pub const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

/// Native token and the wrapped token it claims one for one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CustodialWrap {
    pub native: Address,
    pub wrapped: Address,
}

/// One numeraire and the sizes a round trip may spend, in that token's atoms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Numeraire {
    pub token: Address,
    pub sizes: Vec<BigInt>,
}

/// One stored pair conduit, or the custodial wrap when `block` is absent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    pub token_in: Address,
    pub token_out: Address,
    pub block: Option<u64>,
    pub segments: Vec<SlopeSegment>,
}

/// Wrapped ETH at a hundredth, a tenth, and one ether. USDC at 100, 1,000, and 10,000.
pub fn mainnet_numeraires() -> Vec<Numeraire> {
    vec![
        Numeraire {
            token: WETH,
            sizes: ints(&[10u64.pow(16), 10u64.pow(17), 10u64.pow(18)]),
        },
        Numeraire {
            token: USDC,
            sizes: ints(&[10u64.pow(8), 10u64.pow(9), 10u64.pow(10)]),
        },
    ]
}

fn ints(values: &[u64]) -> Vec<BigInt> {
    values.iter().copied().map(BigInt::from).collect()
}

/// Rank tokens by how many conduits name them.
///
/// Each direction counts for both of its tokens. A token below `floor` is left
/// out. Equal counts keep the lower token. `limit` keeps the highest ranks.
pub fn rank_hubs(edges: &[Edge], floor: usize, limit: usize) -> Vec<Address> {
    let mut score: HashMap<Address, usize> = HashMap::new();
    for edge in edges {
        if edge.token_in == edge.token_out || edge.segments.is_empty() {
            continue;
        }
        *score.entry(edge.token_in).or_default() += 1;
        *score.entry(edge.token_out).or_default() += 1;
    }
    let mut ranked: Vec<(Address, usize)> = score
        .into_iter()
        .filter(|(_, conduits)| *conduits >= floor)
        .collect();
    ranked.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    ranked.truncate(limit);
    ranked.into_iter().map(|(token, _)| token).collect()
}

/// Spots for every token that clears a route into one of `numeraires`.
///
/// Numeraires join the hub list even when they sit below `floor`. `wrap`
/// inserts the one-for-one custodial wrap in both directions when that lane
/// is not already a conduit. The result is ordered by token, then numeraire.
pub fn price(
    edges: &[Edge],
    floor: usize,
    limit: usize,
    numeraires: &[Numeraire],
    wrap: Option<CustodialWrap>,
) -> Vec<Spot> {
    let mut hubs = rank_hubs(edges, floor, limit);
    for numeraire in numeraires {
        if !hubs.contains(&numeraire.token) {
            hubs.push(numeraire.token);
        }
    }
    let book = Book::build(edges, wrap);
    let mut tokens = BTreeSet::new();
    for (from, to) in book.lanes.keys() {
        tokens.insert(*from);
        tokens.insert(*to);
    }
    let touched = both_ways(&book, &hubs);
    let mut spots = Vec::new();
    for token in tokens {
        for numeraire in numeraires {
            if token == numeraire.token {
                continue;
            }
            let Some(hit) = best(&book, &touched, token, numeraire, &hubs) else {
                continue;
            };
            spots.push(Spot {
                token,
                numeraire: numeraire.token,
                size: hit.size,
                bought: hit.bought,
                sold: hit.sold,
                hops: hit.hops,
                block: hit.block,
            });
        }
    }
    spots
}

struct Hit {
    size: BigInt,
    bought: BigInt,
    sold: BigInt,
    hops: u8,
    block: u64,
}

struct Lane {
    block: Option<u64>,
    segments: Vec<SlopeSegment>,
}

struct Book {
    lanes: HashMap<(Address, Address), Lane>,
}

impl Book {
    fn build(edges: &[Edge], wrap: Option<CustodialWrap>) -> Self {
        let mut lanes = HashMap::new();
        for edge in edges {
            if edge.token_in == edge.token_out || edge.segments.is_empty() {
                continue;
            }
            lanes.insert(
                (edge.token_in, edge.token_out),
                Lane {
                    block: edge.block,
                    segments: edge.segments.clone(),
                },
            );
        }
        if let Some(wrap) = wrap {
            if wrap.native != wrap.wrapped {
                let width: BigInt = BigInt::from(1) << 256;
                for (from, to) in [(wrap.native, wrap.wrapped), (wrap.wrapped, wrap.native)] {
                    lanes.entry((from, to)).or_insert_with(|| Lane {
                        block: None,
                        segments: vec![SlopeSegment {
                            width: width.clone(),
                            output: width.clone(),
                            action: routing::Action {
                                kind: 0,
                                pool: [0; 32],
                            },
                        }],
                    });
                }
            }
        }
        Self { lanes }
    }

    fn get(&self, from: Address, to: Address) -> Option<&Lane> {
        self.lanes.get(&(from, to))
    }
}

/// Hubs each token can both spend into and be paid from.
fn both_ways(book: &Book, hubs: &[Address]) -> HashMap<Address, Vec<Address>> {
    let hub_set: HashSet<Address> = hubs.iter().copied().collect();
    let mut out: HashMap<Address, HashSet<Address>> = HashMap::new();
    let mut inn: HashMap<Address, HashSet<Address>> = HashMap::new();
    for (from, to) in book.lanes.keys() {
        if hub_set.contains(to) {
            out.entry(*from).or_default().insert(*to);
        }
        if hub_set.contains(from) {
            inn.entry(*to).or_default().insert(*from);
        }
    }
    let mut both = HashMap::new();
    for (token, outgoing) in out {
        let Some(incoming) = inn.get(&token) else {
            continue;
        };
        let mut hubs: Vec<Address> = outgoing.intersection(incoming).copied().collect();
        hubs.sort();
        if !hubs.is_empty() {
            both.insert(token, hubs);
        }
    }
    both
}

fn best(
    book: &Book,
    touched: &HashMap<Address, Vec<Address>>,
    token: Address,
    numeraire: &Numeraire,
    hubs: &[Address],
) -> Option<Hit> {
    let mut sizes: Vec<&BigInt> = numeraire
        .sizes
        .iter()
        .filter(|size| *size > &BigInt::from(0))
        .collect();
    sizes.sort();
    sizes.dedup();
    let adjacent = touched.get(&token).map(Vec::as_slice).unwrap_or(&[]);
    for size in sizes {
        let mut won: Option<Hit> = None;
        note(
            &mut won,
            round_trip(
                book,
                &[(numeraire.token, token)],
                &[(token, numeraire.token)],
                size,
            ),
        );
        for hub in adjacent {
            if *hub == token || *hub == numeraire.token {
                continue;
            }
            note(
                &mut won,
                round_trip(
                    book,
                    &[(numeraire.token, *hub), (*hub, token)],
                    &[(token, *hub), (*hub, numeraire.token)],
                    size,
                ),
            );
        }
        for left in hubs {
            if *left == token || *left == numeraire.token {
                continue;
            }
            for right in adjacent {
                if *right == token || *right == numeraire.token || right == left {
                    continue;
                }
                note(
                    &mut won,
                    round_trip(
                        book,
                        &[(numeraire.token, *left), (*left, *right), (*right, token)],
                        &[(token, *right), (*right, *left), (*left, numeraire.token)],
                        size,
                    ),
                );
            }
        }
        if won.is_some() {
            return won;
        }
    }
    None
}

fn note(won: &mut Option<Hit>, hit: Option<Hit>) {
    let Some(hit) = hit else {
        return;
    };
    let replace = match won {
        None => true,
        Some(current) => {
            hit.bought > current.bought || (hit.bought == current.bought && hit.hops < current.hops)
        }
    };
    if replace {
        *won = Some(hit);
    }
}

fn round_trip(
    book: &Book,
    buy: &[(Address, Address)],
    sell: &[(Address, Address)],
    size: &BigInt,
) -> Option<Hit> {
    let (bought, buy_block) = through(book, buy, size)?;
    let (sold, sell_block) = through(book, sell, &bought)?;
    if &sold * 2 < *size {
        return None;
    }
    let block = match (buy_block, sell_block) {
        (Some(left), Some(right)) => left.min(right),
        (Some(block), None) | (None, Some(block)) => block,
        (None, None) => 0,
    };
    Some(Hit {
        size: size.clone(),
        bought,
        sold,
        hops: buy.len() as u8,
        block,
    })
}

/// Pay `input` along each lane. Every lane must cover the whole input and pay something.
fn through(
    book: &Book,
    pairs: &[(Address, Address)],
    input: &BigInt,
) -> Option<(BigInt, Option<u64>)> {
    let mut amount = input.clone();
    let mut block: Option<u64> = None;
    for (from, to) in pairs {
        let lane = book.get(*from, *to)?;
        amount = pay(lane, &amount)?;
        block = match (block, lane.block) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (None, Some(right)) => Some(right),
            (Some(left), None) => Some(left),
            (None, None) => None,
        };
    }
    Some((amount, block))
}

fn pay(lane: &Lane, input: &BigInt) -> Option<BigInt> {
    if input <= &BigInt::from(0) {
        return None;
    }
    let mut width = BigInt::from(0);
    for segment in &lane.segments {
        width += &segment.width;
    }
    if &width < input {
        return None;
    }
    let paid = routing::quote(&lane.segments, input);
    if paid <= BigInt::from(0) {
        None
    } else {
        Some(paid)
    }
}
