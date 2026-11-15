//! One EVM log with its place in the chain.
//!
//! The place is the block number, the block hash, the transaction index, the
//! log index, and the transaction hash when the transport had one. The body is
//! the emitter, the topics, and the data.
//!
//! Era1 files and devp2p receipts both become a [`Log`]. A domain that
//! classifies a log or decodes a creation event reads that value and does not
//! learn which transport produced it.
//!
//! [`BlockLogs`] is the logs of one block, in chain order. A stored block holds
//! the same logs as a zstd receipt bundle. An empty body is a hole and yields
//! no logs.

use alloy_primitives::{Address, Bytes, B256};
use reth_ethereum_primitives::Receipt;
use serde::Serialize;
use store_model::{decode_receipt_bundle, decompress_receipt_bundle, BlockRecord};

/// One log, with its place in the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Log {
    pub block_number: u64,
    pub block_hash: B256,
    pub tx_index: u64,
    /// Absent when the transport returned receipts without transaction hashes.
    pub tx_hash: Option<B256>,
    pub log_index: u64,
    pub address: Address,
    pub topics: Vec<B256>,
    pub data: Bytes,
}

/// Logs from a single block, in chain order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLogs {
    pub number: u64,
    pub hash: B256,
    pub logs: Vec<Log>,
}

/// Logs of one block's receipts, in chain order.
///
/// A receipt does not carry its transaction hash, so [`Log::tx_hash`] is absent.
pub fn flatten_logs(block_number: u64, block_hash: B256, receipts: &[Receipt]) -> Vec<Log> {
    let mut logs = Vec::new();
    let mut log_index = 0_u64;
    for (tx_index, receipt) in receipts.iter().enumerate() {
        for log in &receipt.logs {
            logs.push(Log {
                block_number,
                block_hash,
                tx_index: tx_index as u64,
                tx_hash: None,
                log_index,
                address: log.address,
                topics: log.data.topics().to_vec(),
                data: Bytes::copy_from_slice(log.data.data.as_ref()),
            });
            log_index = log_index.saturating_add(1);
        }
    }
    logs
}

/// Logs in a stored block. An empty body is a hole and yields no logs.
pub fn logs_in_block(record: &BlockRecord) -> eyre::Result<Vec<Log>> {
    if record.receipts_zstd.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = decompress_receipt_bundle(&record.receipts_zstd)?;
    let receipts = decode_receipt_bundle(&bytes)?;
    Ok(flatten_logs(record.number, record.hash, &receipts))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, bytes, Log as AlloyLog, LogData};

    fn one_log() -> Receipt {
        Receipt {
            tx_type: Default::default(),
            success: true,
            cumulative_gas_used: 21_000,
            logs: vec![AlloyLog {
                address: address!("00000000000000000000000000000000000000aa"),
                data: LogData::new_unchecked(
                    vec![b256!(
                        "0101010101010101010101010101010101010101010101010101010101010101"
                    )],
                    bytes!("abcd"),
                ),
            }],
        }
    }

    #[test]
    fn flattens_receipt_logs_in_order() {
        let block = b256!("0202020202020202020202020202020202020202020202020202020202020202");
        let logs = flatten_logs(5, block, &[one_log()]);
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].block_number, 5);
        assert_eq!(logs[0].tx_index, 0);
        assert_eq!(logs[0].log_index, 0);
        assert_eq!(logs[0].tx_hash, None);
        assert_eq!(logs[0].data.as_ref(), &bytes!("abcd")[..]);
    }

    #[test]
    fn an_empty_body_is_a_hole() {
        let record = BlockRecord {
            number: 1,
            hash: B256::ZERO,
            parent_hash: B256::ZERO,
            timestamp: 0,
            receipts_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_zstd: Vec::new(),
        };
        let logs = logs_in_block(&record).expect("hole");
        assert!(logs.is_empty());
    }
}
