//! A checked trade, and the hubs and bounds the routes may use.

use std::collections::HashSet;
use std::fmt;

use action::{Token, NATIVE};
use num_bigint::BigUint;
use num_traits::Zero;

/// The walk lists a token's conduits only when there are at most this many.
pub const NEIGHBOR_LIMIT: usize = 64;

/// Most conduits on one walked path.
pub const WALK_DEPTH: usize = 8;

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
