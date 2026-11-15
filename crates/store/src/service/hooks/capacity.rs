//! Snapshot capacity for a stored pool.

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

static CAPACITY: OnceLock<CapacityFn> = OnceLock::new();

/// Install the snapshot-capacity hook. A second call is ignored.
pub fn install_capacity(capacity: CapacityFn) {
    let _ = CAPACITY.set(capacity);
}

pub(crate) fn capacity(kind: u8, id: B256, state: &PoolState) -> eyre::Result<Option<Capacity>> {
    let Some(hook) = CAPACITY.get().copied() else {
        return Err(eyre::eyre!("capacity hook is not installed"));
    };
    Ok(hook(kind, id, state))
}
