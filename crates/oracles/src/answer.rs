//! `AnswerUpdated` and `AggregatorConfirmed` logs.
//!
//! The aggregator emits `AnswerUpdated` from the transmit that stores a round.
//! The round id in the log is the aggregator's round. A zero value is not an answer.

use crate::store::ObservedAnswer;
use alloy_primitives::{Address, B256, I256, U256};

pub const ANSWER_UPDATED: B256 =
    alloy_primitives::b256!("0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f");
pub const AGGREGATOR_CONFIRMED: B256 =
    alloy_primitives::b256!("33745f67a407dcb785417f9c123dd3641479a102674b6e35c1f10975625b90e9");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainLog {
    pub address: Address,
    pub block: u64,
    pub log_index: u64,
    pub topics: Vec<B256>,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseChange {
    pub proxy: Address,
    pub previous: Address,
    pub latest: Address,
    pub block: u64,
}

pub fn decode_answer_updated(log: &ChainLog) -> Option<ObservedAnswer> {
    if log.topics.first() != Some(&ANSWER_UPDATED) || log.topics.len() < 3 || log.data.len() < 32 {
        return None;
    }
    let mut value_word = [0u8; 32];
    value_word.copy_from_slice(log.topics[1].as_slice());
    let value = I256::from_be_bytes(value_word);
    if value == I256::ZERO {
        return None;
    }
    let round = U256::from_be_slice(log.topics[2].as_slice());
    let round_id = u64::try_from(round).ok()?;
    let updated_at = u64::try_from(U256::from_be_slice(&log.data[..32])).ok()?;
    Some(ObservedAnswer {
        aggregator: log.address,
        round_id,
        value,
        updated_at,
        block: log.block,
        log_index: log.log_index,
    })
}

pub fn decode_aggregator_confirmed(log: &ChainLog) -> Option<PhaseChange> {
    if log.topics.first() != Some(&AGGREGATOR_CONFIRMED) || log.topics.len() < 3 {
        return None;
    }
    Some(PhaseChange {
        proxy: log.address,
        previous: address_topic(&log.topics[1])?,
        latest: address_topic(&log.topics[2])?,
        block: log.block,
    })
}

fn address_topic(topic: &B256) -> Option<Address> {
    if topic[..12].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(Address::from_slice(&topic[12..]))
}

/// Inclusive spans of at most `span` blocks.
pub fn block_spans(from: u64, to: u64, span: u64) -> Vec<(u64, u64)> {
    if from > to || span == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cursor = from;
    while cursor <= to {
        let end = cursor.saturating_add(span - 1).min(to);
        out.push((cursor, end));
        if end == to {
            break;
        }
        cursor = end + 1;
    }
    out
}

/// First block of the window that should contain this feed's latest answer.
pub fn answer_window(head: u64, heartbeat: u64) -> u64 {
    let blocks = heartbeat.saturating_div(12).saturating_add(2_000);
    head.saturating_sub(blocks.min(300_000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn answer_updated_topic_is_the_aggregator_event() {
        assert_eq!(
            ANSWER_UPDATED,
            b256!("0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f")
        );
    }

    #[test]
    fn spans_cover_the_range_without_overlap() {
        assert_eq!(
            block_spans(1, 5_000, 2_000),
            vec![(1, 2_000), (2_001, 4_000), (4_001, 5_000)]
        );
        assert!(block_spans(5, 4, 10).is_empty());
    }

    #[test]
    fn the_window_covers_one_heartbeat() {
        assert_eq!(answer_window(100_000, 3_600), 100_000 - (300 + 2_000));
        assert_eq!(answer_window(10, 3_600), 0);
    }
}
