//! Rows the directory, the proxy read, and the answer follow share.
//!
//! The store crate opens the file that holds them.

use crate::abi::ProxyHead;
use crate::directory::{DirectoryError, DirectoryRow};
use alloy_primitives::{Address, I256, U256};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Directory(#[from] DirectoryError),
    #[error("proxy {proxy} is not listed")]
    Unknown { proxy: Address },
    #[error("{0}")]
    Store(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub generation: u64,
    pub named: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    pub row: DirectoryRow,
    pub listed_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configuration {
    pub decimals: u8,
    pub description: String,
    pub version: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub phase_id: u16,
    pub aggregator: Address,
    pub min_answer: Option<I256>,
    pub max_answer: Option<I256>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryJob {
    pub proxy: Address,
    pub phase_id: u16,
    pub aggregator: Address,
}

/// Counts the status view reads for the feed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gauges {
    pub feeds: u64,
    pub configured: u64,
    pub answers: u64,
    pub jobs: u64,
    pub follow: Option<u64>,
    pub lookback: bool,
}

/// One answer taken from an aggregator `AnswerUpdated` log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub proxy: Address,
    pub phase_id: u16,
    pub round_id: u64,
    pub value: I256,
    pub updated_at: u64,
    pub block: u64,
    pub log_index: u64,
}

/// A log the follow pass has already decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedAnswer {
    pub aggregator: Address,
    pub round_id: u64,
    pub value: I256,
    pub updated_at: u64,
    pub block: u64,
    pub log_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    Configuration {
        stored_decimals: u8,
        observed_decimals: u8,
        stored_description: String,
        observed_description: String,
        stored_version: U256,
        observed_version: U256,
    },
    Phase {
        newest: u16,
        observed: u16,
    },
    Aggregator {
        phase_id: u16,
        stored: Address,
        observed: Address,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    Failed(String),
    Configured {
        head: ProxyHead,
        min_answer: Option<I256>,
        max_answer: Option<I256>,
    },
}
