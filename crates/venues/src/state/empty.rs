//! Pools whose snapshot cannot price a positive input.

use alloy_primitives::B256;
use store_model::METRIC;

use crate::fit::has_trade_size;
use crate::Catalog;

/// Delete pools whose stored snapshot cannot price any positive input.
///
/// Pools with no snapshot yet stay, so a fill can be ingested afterwards.
/// A one-wei reserve stays. Zero reserves and an empty book do not.
pub fn drop_empty_pools(catalog: &Catalog) -> eyre::Result<Vec<(u8, B256)>> {
    let mut drop = Vec::new();
    for kind in 1..=METRIC {
        let mut after = None;
        loop {
            let page = catalog.page_venues(kind, after, 64)?;
            if page.is_empty() {
                break;
            }
            after = Some(page.last().expect("page").id);
            for row in &page {
                let state = catalog.read_state(row.kind, row.id)?;
                if !has_trade_size(row.kind, row.id, state) {
                    drop.push((row.kind, row.id));
                }
            }
            if page.len() < 64 {
                break;
            }
        }
    }
    let (_, curves) = catalog.delete_pools(&drop)?;
    crate::conduit::forget_curves(catalog, &curves)?;
    Ok(drop)
}
