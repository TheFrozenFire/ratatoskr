//! Pool row bytes.

use store_model::PoolRecord;

use super::RECORD_VERSION;

pub(super) fn encode_row(row: &PoolRecord) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 20 * 3 + 4 + 4 + 8 + 32 + 8 + 8);
    out.push(RECORD_VERSION);
    out.extend_from_slice(row.token0.as_slice());
    out.extend_from_slice(row.token1.as_slice());
    out.extend_from_slice(&row.fee.to_le_bytes());
    out.extend_from_slice(&row.tick_spacing.to_le_bytes());
    out.extend_from_slice(row.factory.as_slice());
    out.extend_from_slice(&row.block_number.to_le_bytes());
    out.extend_from_slice(row.block_hash.as_slice());
    out.extend_from_slice(&row.tx_index.to_le_bytes());
    out.extend_from_slice(&row.log_index.to_le_bytes());
    out
}
