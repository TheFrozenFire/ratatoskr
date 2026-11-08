//! Log jobs for venues that keep no index.
//!
//! The header chain already stores every logs bloom. History reads those
//! blooms locally, then fetches receipts only for the hits.

use alloy_primitives::{Address, B256};

use crate::decode::{self, known, BAL_V2_POOL, BAL_V2_TOKENS, BAL_V3_POOL, METRIC_CREATED};
/// First post-merge block. era1 stops on the block before this.
pub(crate) const MERGE_BLOCK: u64 = 15_537_394;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Walk {
    Done,
    Stopped { at: u64, reason: String },
}

pub struct LogJob {
    pub name: String,
    pub address: Address,
    pub topics: Vec<B256>,
    pub deploy_block: u64,
}

pub fn jobs() -> Vec<LogJob> {
    let known = known();
    vec![
        log_job(
            "logs:uni-v3",
            known.uni_v3,
            vec![decode::topic_pool()],
            12_369_621,
        ),
        log_job(
            "logs:pancake-v3",
            known.pancake_v3,
            vec![decode::topic_pool()],
            16_950_686,
        ),
        log_job(
            "logs:sushi-v3",
            known.sushi_v3,
            vec![decode::topic_pool()],
            16_955_547,
        ),
        log_job(
            "logs:v3:0x88695a239a0a3f70729c4cb30fd32f9b4978513d",
            known.raw_v3_a,
            vec![decode::topic_pool()],
            25_267_630,
        ),
        log_job(
            "logs:v3:0x956920f97690bdd76a6a10c7ba931dec14f61169",
            known.raw_v3_b,
            vec![decode::topic_pool()],
            26_254_054,
        ),
        log_job(
            "logs:uni-v4",
            known.pool_manager,
            vec![decode::topic_v4()],
            21_688_329,
        ),
        log_job(
            "logs:balancer-v2",
            known.bal_v2,
            vec![BAL_V2_POOL, BAL_V2_TOKENS],
            12_272_146,
        ),
        log_job(
            "logs:balancer-v3",
            known.bal_v3,
            vec![BAL_V3_POOL],
            21_332_121,
        ),
        log_job(
            "logs:metric",
            known.metric,
            vec![METRIC_CREATED],
            25_984_373,
        ),
    ]
}

fn log_job(name: &str, address: Address, topics: Vec<B256>, deploy_block: u64) -> LogJob {
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
