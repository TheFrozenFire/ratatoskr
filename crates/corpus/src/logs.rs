//! Receipt bundle stored inside a [`BlockRecord`](model::BlockRecord).
//!
//! The zstd payload is a little-endian receipt count, then for each receipt a
//! little-endian length and the EIP-2718 encoding, bloom included.

use alloy_consensus::{ReceiptWithBloom, TxReceipt};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Bytes, B256};
use eyre::eyre;
use model::{BlockRecord, NormalizedLog};
use reth_ethereum_primitives::Receipt;

pub fn encode_receipt_bundle(receipts: &[Receipt]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(receipts.len() as u32).to_le_bytes());
    for receipt in receipts {
        let encoded = receipt.clone().into_with_bloom().encoded_2718();
        let len = u32::try_from(encoded.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&encoded[..len as usize]);
    }
    out
}

pub fn decode_receipt_bundle(mut bytes: &[u8]) -> eyre::Result<Vec<Receipt>> {
    if bytes.len() < 4 {
        return Err(eyre!("receipt bundle truncated"));
    }
    let count = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
    bytes = &bytes[4..];
    let mut receipts = Vec::with_capacity(count);
    for _ in 0..count {
        if bytes.len() < 4 {
            return Err(eyre!("receipt length truncated"));
        }
        let len = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        bytes = &bytes[4..];
        if bytes.len() < len {
            return Err(eyre!("receipt bytes truncated"));
        }
        let wrapped = ReceiptWithBloom::<Receipt>::decode_2718(&mut &bytes[..len])
            .map_err(|err| eyre!("receipt decode: {err}"))?;
        receipts.push(wrapped.receipt);
        bytes = &bytes[len..];
    }
    Ok(receipts)
}

/// Logs in a stored block. An empty body is a hole and yields no logs.
pub fn logs_in_block(record: &BlockRecord) -> eyre::Result<Vec<NormalizedLog>> {
    if record.receipts_zstd.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = zstd::stream::decode_all(record.receipts_zstd.as_slice())
        .map_err(|err| eyre!("zstd: {err}"))?;
    let receipts = decode_receipt_bundle(&bytes)?;
    Ok(flatten_logs(record.number, record.hash, &receipts))
}

pub fn flatten_logs(
    block_number: u64,
    block_hash: B256,
    receipts: &[Receipt],
) -> Vec<NormalizedLog> {
    let mut logs = Vec::new();
    let mut log_index = 0_u64;
    for (tx_index, receipt) in receipts.iter().enumerate() {
        for log in &receipt.logs {
            logs.push(NormalizedLog {
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256, bytes, Log, LogData};

    fn one_log() -> Receipt {
        Receipt {
            tx_type: Default::default(),
            success: true,
            cumulative_gas_used: 21_000,
            logs: vec![Log {
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
    fn receipt_bundle_roundtrip() {
        let receipt = one_log();
        let decoded =
            decode_receipt_bundle(&encode_receipt_bundle(&[receipt.clone()])).expect("bundle");
        assert_eq!(decoded, vec![receipt]);
    }
}
