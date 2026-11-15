//! Snapshot capacity and the empty-pool check.

use alloy_primitives::{B256, U256};
use std::sync::OnceLock;
use store_model::PoolState;

/// Trade size of one stored snapshot. `top` is the largest priced input.
pub struct Capacity {
    pub any_positive: bool,
    pub fills_sample: bool,
    pub top: U256,
}

type CapacityFn = fn(u8, B256, &PoolState) -> Option<Capacity>;
type TradeSizeFn = fn(u8, B256, PoolState) -> bool;

static CAPACITY: OnceLock<CapacityFn> = OnceLock::new();
static TRADE_SIZE: OnceLock<TradeSizeFn> = OnceLock::new();

/// Install the snapshot-capacity and empty-pool hooks. A second call is ignored.
pub fn install_capacity(capacity: CapacityFn, trade_size: TradeSizeFn) {
    let _ = CAPACITY.set(capacity);
    let _ = TRADE_SIZE.set(trade_size);
}

pub(crate) fn capacity(kind: u8, id: B256, state: &PoolState) -> eyre::Result<Option<Capacity>> {
    let Some(hook) = CAPACITY.get().copied() else {
        return Err(eyre::eyre!("capacity hook is not installed"));
    };
    Ok(hook(kind, id, state))
}

pub(crate) fn has_trade_size(kind: u8, id: B256, state: PoolState) -> eyre::Result<bool> {
    let Some(hook) = TRADE_SIZE.get().copied() else {
        return Err(eyre::eyre!("trade size hook is not installed"));
    };
    Ok(hook(kind, id, state))
}
