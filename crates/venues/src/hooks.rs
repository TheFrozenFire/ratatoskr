//! Installs the catalog hooks `store` calls back into this crate.

use alloy_primitives::{Address, B256};
use store::service::Capacity;
use store_model::{CurveRecord, PoolState};

/// Install curve and capacity hooks. Call this before [`crate::Catalog::open`].
pub fn install_catalog_hooks() {
    store::service::install_capacity(capacity_hook, trade_size_hook);
    store::service::install_segments(priced_pairs, segments_hook);
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

fn trade_size_hook(kind: u8, id: B256, state: PoolState) -> bool {
    crate::fit::has_trade_size(kind, id, state)
}

fn priced_pairs(curve: &CurveRecord) -> Vec<(Address, Address)> {
    if curve.version != store_model::CURVE_VERSION {
        return Vec::new();
    }
    let Ok(fit) = fitter::decode_fit(&curve.payload) else {
        return Vec::new();
    };
    if fit.reason != 0 {
        return Vec::new();
    }
    let mut pairs = Vec::new();
    for direction in fit.directions {
        if direction.token_in == direction.token_out {
            continue;
        }
        let pair = (direction.token_in, direction.token_out);
        if !pairs.contains(&pair) {
            pairs.push(pair);
        }
    }
    pairs
}

fn segments_hook(rows: &store::service::DirectionRows<'_>) -> Option<Vec<routing::SlopeSegment>> {
    let detail = crate::state::join_detail(
        rows.immutable,
        rows.mutable_body,
        rows.ticks,
        rows.kind,
        rows.id,
    )
    .ok()?;
    let knots = crate::fit::direction_knots(&detail, rows.curve, rows.token_in, rows.token_out);
    let action = routing::Action {
        kind: rows.kind,
        pool: rows.id.0,
    };
    Some(routing::segments_from_knots(&knots, action))
}
