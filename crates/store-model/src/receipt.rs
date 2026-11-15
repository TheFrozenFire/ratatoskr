//! Receipt bundle stored in [`BlockRecord`](crate::BlockRecord).
//!
//! The bytes are a little-endian receipt count, then for each receipt a
//! little-endian length and the EIP-2718 encoding, bloom included. The block
//! record stores those bytes inside a zstd frame at level 3.

use alloy_consensus::{ReceiptWithBloom, TxReceipt};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use eyre::eyre;
use reth_ethereum_primitives::Receipt;

/// Level used for a receipt frame stored on a block record.
pub const RECEIPT_ZSTD_LEVEL: i32 = 3;

pub fn compress_receipt_bundle(raw: &[u8]) -> eyre::Result<Vec<u8>> {
    zstd::bulk::compress(raw, RECEIPT_ZSTD_LEVEL).map_err(|err| eyre!("zstd: {err}"))
}

pub fn decompress_receipt_bundle(frame: &[u8]) -> eyre::Result<Vec<u8>> {
    zstd::stream::decode_all(frame).map_err(|err| eyre!("zstd: {err}"))
}

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
    fn receipt_bundle_roundtrip() {
        let receipt = one_log();
        let decoded = decode_receipt_bundle(&encode_receipt_bundle(std::slice::from_ref(&receipt)))
            .expect("bundle");
        assert_eq!(decoded, vec![receipt]);
    }
}
