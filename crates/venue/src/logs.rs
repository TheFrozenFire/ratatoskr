//! Log jobs for venues that keep no index.
//!
//! The header chain already stores every logs bloom. History reads those
//! blooms locally, then fetches receipts only for the hits.

use alloy_primitives::{Address, B256};

/// First post-merge block. era1 stops on the block before this.
pub(crate) const MERGE_BLOCK: u64 = 15_537_394;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Walk {
    Done,
    Stopped { at: u64, reason: String },
}

#[derive(Clone)]
pub struct LogJob {
    pub name: String,
    pub address: Address,
    pub topics: Vec<B256>,
    pub deploy_block: u64,
}

pub(crate) fn log_job(
    name: &str,
    address: Address,
    topics: Vec<B256>,
    deploy_block: u64,
) -> LogJob {
    LogJob {
        name: name.into(),
        address,
        topics,
        deploy_block,
    }
}

pub(crate) fn job_filter(job: &LogJob) -> filter::LogFilter {
    filter::LogFilter {
        addresses: vec![job.address],
        topics: [job.topics.clone(), Vec::new(), Vec::new(), Vec::new()],
    }
}
