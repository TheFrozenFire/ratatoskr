//! Re-read stored pools through the detail lens.

use super::{
    concentrated_kind, curve_for_snapshot, detail_flags, split_detail, Catalog, ImmutableRow,
    PoolDetail, CLASS_HEAD, FLAG_FAILED,
};
use chain::rpc::Rpc;

/// Pools re-read through the detail lens, and curves written from those snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshReport {
    pub read: u64,
    pub failed: u64,
    pub curves: u64,
}

/// Re-read `kinds` through the detail lens and store a curve from each snapshot.
///
/// The read uses one chain head. A failed read leaves the stored snapshot alone.
/// A concentrated pool whose tick book is not kept is counted in `failed` and
/// stores no curve. A snapshot whose quote has no positive output stores an
/// empty curve, and the old conduit members drop.
pub fn refresh_kinds(catalog: &Catalog, rpc: &Rpc, kinds: &[u8]) -> eyre::Result<RefreshReport> {
    let block = rpc.block_number()?;
    if block == 0 {
        return Err(eyre::eyre!("snapshot block is 0"));
    }
    let mut report = RefreshReport {
        read: 0,
        failed: 0,
        curves: 0,
    };
    for &kind in kinds {
        let mut after = None;
        loop {
            let page = catalog.page_venues(kind, after, 16)?;
            if page.is_empty() {
                break;
            }
            let full = page.len() == 16;
            after = page.last().map(|row| row.id);
            let details = chain::detail::load_details_at(rpc, &page, Some(block))?;
            if details.len() != page.len() {
                return Err(eyre::eyre!("detail lens row count"));
            }
            for (row, detail) in page.iter().zip(details) {
                if detail_flags(&detail) & FLAG_FAILED != 0 {
                    report.failed += 1;
                    continue;
                }
                let (immutable, mut mutable) =
                    split_detail(&detail).map_err(|err| eyre::eyre!(err))?;
                mutable.block = block;
                mutable.class = CLASS_HEAD;
                let state = catalog.read_state(row.kind, row.id)?;
                if state.immutable.is_none() {
                    catalog.put_immutable(&[ImmutableRow {
                        kind: row.kind,
                        id: row.id,
                        body: immutable,
                    }])?;
                }
                let stored = catalog.put_mutable(&mutable)?;
                let mut detail = detail;
                if stored.kept_ticks {
                    let fresh = catalog.read_state(row.kind, row.id)?;
                    if let PoolDetail::Concentrated(pool) = &mut detail {
                        pool.ticks = fresh.ticks;
                    }
                }
                let needs_book =
                    concentrated_kind(row.kind) && mutable.tick_count > 0 && !stored.kept_ticks;
                if needs_book {
                    report.failed += 1;
                    continue;
                }
                let curve = curve_for_snapshot(&detail, stored.generation)
                    .map_err(|err| eyre::eyre!(err))?;
                catalog.put_curve(&curve)?;
                report.read += 1;
                report.curves += 1;
            }
            observability::info!(
                kind = crate::kind_name(kind),
                read = report.read,
                failed = report.failed,
                curves = report.curves,
                "refresh"
            );
            if !full {
                break;
            }
        }
    }
    Ok(report)
}
