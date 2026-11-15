//! Installs the catalog hooks `store` calls back into this crate.

use alloy_primitives::B256;
use store::service::Capacity;
use store_model::PoolState;

/// Install the capacity hook. Call this before [`crate::Catalog::open`].
pub fn install_catalog_hooks() {
    store::service::install_capacity(capacity_hook);
}

fn capacity_hook(kind: u8, id: B256, state: &PoolState) -> Option<Capacity> {
    let immutable = state.immutable.as_ref()?;
    let mutable = state.mutable.as_ref()?;
    let detail =
        crate::state::join_detail(immutable, &mutable.body, &state.ticks, kind, id).ok()?;
    let cap = crate::fit::trade_capacity(&detail);
    Some(Capacity {
        any_positive: cap.any_positive,
        fills_sample: cap.fills_sample,
        top: cap.top,
    })
}
