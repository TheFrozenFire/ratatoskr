//! Feed directory rows, proxy configuration, phases, and answers.

use alloy_primitives::{Address, I256, U256};

/// One row of the Ethereum mainnet feed directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryRow {
    pub proxy: Address,
    pub name: String,
    pub path: String,
    pub category: String,
    pub feed_type: String,
    pub contract_version: u64,
    pub decimals: u8,
    pub multiply: String,
    pub heartbeat: u64,
    pub threshold: String,
    pub base_asset: String,
    pub quote_asset: String,
    pub product_type: String,
    pub product_type_code: String,
    pub attribute_type: String,
    pub market_hours: String,
    pub shutdown_date: String,
    pub clic_product_name: String,
    pub hidden: bool,
    pub secondary_proxy: Option<Address>,
    pub svr_label: String,
    pub directory_aggregator: Address,
}

/// The words a proxy call returns for the current phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyHead {
    pub decimals: u8,
    pub description: String,
    pub version: U256,
    pub aggregator: Address,
    pub phase_id: u16,
}

/// One apply of the directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub generation: u64,
    pub named: usize,
}

/// A directory row and the generation that last wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    pub row: DirectoryRow,
    pub listed_generation: u64,
}

/// Decimals, description, and version written once for a proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configuration {
    pub decimals: u8,
    pub description: String,
    pub version: U256,
}

/// One chain phase of a proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub phase_id: u16,
    pub aggregator: Address,
    pub min_answer: Option<I256>,
    pub max_answer: Option<I256>,
}

/// An unclaimed history job for one phase.
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

/// A later read that disagreed with the stored configuration or phase.
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

/// The outcome of one proxy read, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    Failed(String),
    Configured {
        head: ProxyHead,
        min_answer: Option<I256>,
        max_answer: Option<I256>,
    },
}

/// Rows one proxy read writes.
///
/// `Configured` clears a stored failure. `Failure` writes the text and leaves
/// phases, configuration, and jobs in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhasePlan {
    Failure(String),
    Configured {
        configuration: Option<Configuration>,
        mismatches: Vec<Mismatch>,
        phase: Option<Phase>,
    },
}

/// One answer row to insert for a phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerPut {
    pub proxy: Address,
    pub phase_id: u16,
    pub observed: ObservedAnswer,
}
