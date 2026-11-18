//! Pair conduits. Membership and the slope merge are decided here.
//! The table writes stay in [`store::service`].

pub use store_model::{ConduitFail, ConduitJob};

use alloy_primitives::{Address, B256};
use store::service::{ConduitBody, ConduitMember, MemberTarget};
use store::{ConduitStore, VenueStore};
use store_model::{CurveRecord, MutableStored, MutableWrite, PoolState, CURVE_VERSION};

use crate::fit::direction_knots;
use crate::state::{join_detail, unix_now};
use crate::Catalog;

const FILL_PAGE: usize = 1024;
const CONDUIT_BATCH: u32 = 32;
const CONDUIT_LEASE_SECS: u64 = 120;
const CONDUIT_BACKOFF_SECS: u64 = 60;
const SCAN_LOG: u64 = 100_000;

/// Ordered pairs this curve prices. An undecodable row prices nothing.
pub(crate) fn priced_pairs(curve: &CurveRecord) -> Vec<(Address, Address)> {
    if curve.version != CURVE_VERSION {
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

struct Built {
    block: u64,
    generation: u64,
    segments: Vec<routing::SlopeSegment>,
}

fn segments_of(
    state: &PoolState,
    kind: u8,
    id: B256,
    generation: u64,
    token_in: Address,
    token_out: Address,
) -> Option<Built> {
    let (Some(immutable), Some(mutable), Some(curve)) = (
        state.immutable.as_ref(),
        state.mutable.as_ref(),
        state.curve.as_ref(),
    ) else {
        return None;
    };
    if curve.version != CURVE_VERSION
        || curve.generation != mutable.generation
        || curve.generation != generation
    {
        return None;
    }
    if !priced_pairs(curve).contains(&(token_in, token_out)) {
        return None;
    }
    let detail = join_detail(immutable, &mutable.body, &state.ticks, kind, id).ok()?;
    let knots = direction_knots(&detail, curve, token_in, token_out);
    let segments = routing::segments_from_knots(&knots, routing::Action { kind, pool: id.0 });
    Some(Built {
        block: mutable.block,
        generation: curve.generation,
        segments,
    })
}

fn retarget(
    catalog: &Catalog,
    kind: u8,
    id: B256,
    previous: Option<&CurveRecord>,
    next: Option<&CurveRecord>,
) -> eyre::Result<()> {
    let old_pairs = previous.map(priced_pairs).unwrap_or_default();
    let new_pairs = next.map(priced_pairs).unwrap_or_default();
    let generation = next.map(|curve| curve.generation);
    catalog.apply_retarget(kind, id, &old_pairs, &new_pairs, generation)
}

async fn retarget_store<S: ConduitStore>(
    store: &S,
    kind: u8,
    id: B256,
    previous: Option<&CurveRecord>,
    next: Option<&CurveRecord>,
) -> eyre::Result<()> {
    let old_pairs = previous.map(priced_pairs).unwrap_or_default();
    let new_pairs = next.map(priced_pairs).unwrap_or_default();
    let generation = next.map(|curve| curve.generation);
    store
        .apply_retarget(kind, id, &old_pairs, &new_pairs, generation)
        .await
}

/// Write a snapshot, then drop conduit members of the curve it removed.
pub fn commit_mutable(catalog: &Catalog, row: &MutableWrite) -> eyre::Result<MutableStored> {
    let stored = catalog.put_mutable(row)?;
    retarget(
        catalog,
        row.kind,
        row.id,
        stored.previous_curve.as_ref(),
        None,
    )?;
    Ok(stored)
}

/// Write a snapshot, then drop conduit members of the curve it removed.
pub async fn commit_mutable_store<S: VenueStore + ConduitStore>(
    store: &S,
    row: &MutableWrite,
) -> eyre::Result<MutableStored> {
    let stored = store.put_mutable(row).await?;
    retarget_store(
        store,
        row.kind,
        row.id,
        stored.previous_curve.as_ref(),
        None,
    )
    .await?;
    Ok(stored)
}

/// Write a curve, then retarget conduit members from the row it replaced.
pub fn commit_curve(catalog: &Catalog, curve: &CurveRecord) -> eyre::Result<()> {
    let previous = catalog.put_curve(curve)?;
    retarget(
        catalog,
        curve.kind,
        curve.id,
        previous.as_ref(),
        Some(curve),
    )
}

/// Write a curve, then retarget conduit members from the row it replaced.
pub async fn commit_curve_store<S: VenueStore + ConduitStore>(
    store: &S,
    curve: &CurveRecord,
) -> eyre::Result<()> {
    let previous = store.put_curve(curve).await?;
    retarget_store(store, curve.kind, curve.id, previous.as_ref(), Some(curve)).await
}

/// Write a page of curves, then retarget each replaced row.
pub fn commit_curves(catalog: &Catalog, curves: &[CurveRecord]) -> eyre::Result<()> {
    let replaced = catalog.put_curves(curves)?;
    for (curve, previous) in curves.iter().zip(replaced) {
        retarget(
            catalog,
            curve.kind,
            curve.id,
            previous.as_ref(),
            Some(curve),
        )?;
    }
    Ok(())
}

/// Drop conduit members for curves removed with their pools.
pub fn forget_curves(catalog: &Catalog, curves: &[CurveRecord]) -> eyre::Result<()> {
    for curve in curves {
        retarget(catalog, curve.kind, curve.id, Some(curve), None)?;
    }
    Ok(())
}

/// Delete a stride of listed identities and drop the conduit members of removed curves.
pub fn trim_listed(
    catalog: &Catalog,
    target: u64,
    listed: &[(u8, B256)],
) -> eyre::Result<(u64, Vec<(u8, B256)>)> {
    let (removed, left, curves) = catalog.trim_listed(target, listed)?;
    forget_curves(catalog, &curves)?;
    Ok((removed, left))
}

fn absorb(curves: &mut u64, targets: &mut Vec<MemberTarget>, page: &store::service::CurvePage) {
    for row in &page.rows {
        *curves += 1;
        if curves.is_multiple_of(SCAN_LOG) {
            observability::info!(curves = *curves, "conduit fill scan");
        }
        let Some(curve) = &row.curve else {
            observability::warn!(
                kind = row.kind,
                id = %row.id,
                "conduit fill skipped a curve row"
            );
            continue;
        };
        for (token_in, token_out) in priced_pairs(curve) {
            targets.push(MemberTarget {
                token_in,
                token_out,
                kind: curve.kind,
                id: curve.id,
                generation: curve.generation,
            });
        }
    }
}

/// Queue one job per ordered pair whose conduit is missing or behind the stored curves.
///
/// The first call reads every curve. Later calls retarget only curves written
/// since that fill whose member update did not commit.
#[allow(dead_code)]
pub fn enqueue_conduit_fill(catalog: &Catalog) -> eyre::Result<u64> {
    if catalog.member_fill_done()? {
        let pending = catalog.pending_member_curves()?;
        let curves = pending.len() as u64;
        for row in pending {
            retarget(
                catalog,
                row.kind,
                row.id,
                row.previous.as_ref(),
                row.current.as_ref(),
            )?;
        }
        if curves > 0 {
            observability::info!(curves, "conduit fill repaired");
        }
        return Ok(0);
    }
    let mut curves = 0_u64;
    let mut targets = Vec::new();
    let mut after = None;
    loop {
        let page = catalog.page_curves(after, FILL_PAGE)?;
        absorb(&mut curves, &mut targets, &page);
        if !page.more {
            break;
        }
        let Some(resume) = page.resume else {
            break;
        };
        after = Some(resume);
    }
    observability::info!(curves, members = targets.len(), "conduit fill scanned");
    let queued = catalog.enqueue_members(&targets)?;
    catalog.mark_member_fill_done()?;
    Ok(queued)
}

/// Queue one job per ordered pair whose conduit is missing or behind the stored curves.
///
/// The first call reads every curve. Later calls retarget only curves written
/// since that fill whose member update did not commit.
pub async fn enqueue_conduit_fill_store<S: ConduitStore>(store: &S) -> eyre::Result<u64> {
    if store.member_fill_done().await? {
        let pending = store.pending_member_curves().await?;
        let curves = pending.len() as u64;
        for row in pending {
            retarget_store(
                store,
                row.kind,
                row.id,
                row.previous.as_ref(),
                row.current.as_ref(),
            )
            .await?;
        }
        if curves > 0 {
            observability::info!(curves, "conduit fill repaired");
        }
        return Ok(0);
    }
    let queued = scan_members_store(store).await?;
    store.mark_member_fill_done().await?;
    Ok(queued)
}

async fn scan_members_store<S: ConduitStore>(store: &S) -> eyre::Result<u64> {
    let mut curves = 0_u64;
    let mut targets = Vec::new();
    let mut after = None;
    loop {
        let page = store.page_curves(after, FILL_PAGE).await?;
        absorb(&mut curves, &mut targets, &page);
        if !page.more {
            break;
        }
        let Some(resume) = page.resume else {
            break;
        };
        after = Some(resume);
    }
    observability::info!(curves, members = targets.len(), "conduit fill scanned");
    store.enqueue_members(&targets).await
}

fn assemble(
    token_in: Address,
    token_out: Address,
    snapshot: &[ConduitMember],
    states: &[PoolState],
) -> ConduitBody {
    let mut kept = Vec::new();
    let mut parts = Vec::new();
    let mut min_block = u64::MAX;
    let mut max_gen = 0_u64;
    for (member, state) in snapshot.iter().zip(states) {
        let Some(built) = segments_of(
            state,
            member.kind,
            member.id,
            member.generation,
            token_in,
            token_out,
        ) else {
            continue;
        };
        min_block = min_block.min(built.block);
        max_gen = max_gen.max(built.generation);
        parts.push(built.segments);
        kept.push(member.key);
    }
    ConduitBody {
        token_in,
        token_out,
        snapshot: snapshot.to_vec(),
        kept,
        block: min_block,
        generation: max_gen,
        segments: routing::merge(&parts),
    }
}

/// Merge the members of one ordered pair and store the slope segments.
///
/// The bool asks for another pass. It is set when the member index moved
/// while this merge was reading.
pub fn build_conduit(
    catalog: &Catalog,
    token_in: Address,
    token_out: Address,
) -> eyre::Result<bool> {
    let snapshot = catalog.conduit_members(token_in, token_out)?;
    let mut states = Vec::with_capacity(snapshot.len());
    for member in &snapshot {
        states.push(catalog.read_state(member.kind, member.id)?);
    }
    let body = assemble(token_in, token_out, &snapshot, &states);
    catalog.store_conduit(&body)
}

/// Merge the members of one ordered pair and store the slope segments.
///
/// The bool asks for another pass. It is set when the member index moved
/// while this merge was reading.
pub async fn build_conduit_store<S: VenueStore + ConduitStore>(
    store: &S,
    token_in: Address,
    token_out: Address,
) -> eyre::Result<bool> {
    let snapshot = store.conduit_members(token_in, token_out).await?;
    let mut states = Vec::with_capacity(snapshot.len());
    for member in &snapshot {
        states.push(store.read_state(member.kind, member.id).await?);
    }
    let body = assemble(token_in, token_out, &snapshot, &states);
    store.store_conduit(&body).await
}

/// Build every conduit job that is ready. A job that fails stays for a later pass.
pub fn drain_conduits(catalog: &Catalog) -> eyre::Result<u64> {
    let mut built = 0_u64;
    loop {
        let now = unix_now();
        let jobs = catalog.claim_conduits(CONDUIT_BATCH, now, CONDUIT_LEASE_SECS)?;
        if jobs.is_empty() {
            return Ok(built);
        }
        let mut done = Vec::new();
        let mut again = Vec::new();
        let mut failed = Vec::new();
        for job in &jobs {
            match build_conduit(catalog, job.token_in, job.token_out) {
                Ok(true) => {
                    again.push((job.token_in, job.token_out));
                    done.push((job.token_in, job.token_out));
                }
                Ok(false) => done.push((job.token_in, job.token_out)),
                Err(err) => failed.push(ConduitFail {
                    token_in: job.token_in,
                    token_out: job.token_out,
                    not_before: now.saturating_add(CONDUIT_BACKOFF_SECS),
                    error: clip_error(&err.to_string()),
                }),
            }
        }
        catalog.finish_conduits(&done, &again, &failed)?;
        built += done.len() as u64;
        if built.is_multiple_of(256) {
            observability::info!(built, "conduit drain");
        }
    }
}

fn clip_error(error: &str) -> String {
    let mut end = error.len().min(180);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
}

#[cfg(test)]
mod tests {
    use crate::estimate;
    use crate::fit::direction_knots;
    use crate::install_catalog_hooks;
    use crate::state::CurveRecord;
    use crate::state::{derive_curve, split_detail, ImmutableRow, CURVE_VERSION};
    use crate::Catalog;
    use alloy_primitives::{address, U256};
    use alloy_primitives::{Address, B256};
    use chain::detail::{ConstantProduct, PoolDetail};
    use num_bigint::{BigInt, Sign};
    use routing::quote;
    use routing::{segments_from_knots, Action, SlopeSegment};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use store_model::UNI_V2;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-conduit-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn product(byte: u8, reserve0: u64, reserve1: u64) -> PoolDetail {
        let mut id = B256::ZERO;
        id[31] = byte;
        PoolDetail::ConstantProduct(ConstantProduct {
            kind: UNI_V2,
            id,
            pair: address!("1111111111111111111111111111111111111111"),
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            reserve0: U256::from(reserve0),
            reserve1: U256::from(reserve1),
            fee_pips: Some(3000),
            timestamp: 4,
            flags: 0,
        })
    }

    fn write_pool(catalog: &Catalog, detail: &PoolDetail, block: u64) -> CurveRecord {
        let PoolDetail::ConstantProduct(pool) = detail else {
            unreachable!();
        };
        let (body, mut write) = split_detail(detail).unwrap();
        write.block = block;
        catalog
            .put_immutable(&[ImmutableRow {
                kind: pool.kind,
                id: pool.id,
                body,
            }])
            .unwrap();
        let stored = super::commit_mutable(catalog, &write).unwrap();
        let curve = derive_curve(detail, stored.generation).unwrap();
        assert_eq!(curve.version, CURVE_VERSION);
        super::commit_curve(catalog, &curve).unwrap();
        curve
    }

    #[test]
    fn a_later_fill_retargets_a_curve_whose_member_update_did_not_commit() {
        let dir = TempDir::new();
        install_catalog_hooks();
        let catalog = Catalog::open(&dir.0).unwrap();
        let curve = write_pool(&catalog, &product(1, 1_000_000, 1_000_000), 10);
        assert_eq!(super::enqueue_conduit_fill(&catalog).unwrap(), 2);
        let jobs = catalog.claim_conduits(8, 1, 30).unwrap();
        let done: Vec<_> = jobs
            .iter()
            .map(|job| (job.token_in, job.token_out))
            .collect();
        catalog.finish_conduits(&done, &[], &[]).unwrap();
        assert!(catalog.claim_conduits(8, 1, 30).unwrap().is_empty());
        catalog.put_curve(&curve).unwrap();
        assert_eq!(catalog.pending_member_curves().unwrap().len(), 1);
        assert_eq!(super::enqueue_conduit_fill(&catalog).unwrap(), 0);
        assert!(catalog.pending_member_curves().unwrap().is_empty());
        assert!(!catalog.claim_conduits(8, 1, 30).unwrap().is_empty());
    }

    #[test]
    fn a_pair_conduit_merges_two_constant_product_pools() {
        let dir = TempDir::new();
        install_catalog_hooks();
        let catalog = Catalog::open(&dir.0).unwrap();
        let deep = product(1, 1_000_000, 1_000_000);
        let thin = product(2, 2_000_000, 500_000);
        let deep_curve = write_pool(&catalog, &deep, 10);
        let thin_curve = write_pool(&catalog, &thin, 12);
        let token_in = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token_out = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        assert_eq!(catalog.state_summary().unwrap().jobs, 2);
        assert_eq!(super::enqueue_conduit_fill(&catalog).unwrap(), 2);
        assert_eq!(catalog.state_summary().unwrap().jobs, 2);
        let first = catalog.claim_conduits(8, 1, 30).unwrap();
        assert_eq!(first.len(), 2);
        assert!(catalog.claim_conduits(8, 1, 30).unwrap().is_empty());

        let mut again = Vec::new();
        for job in &first {
            if super::build_conduit(&catalog, job.token_in, job.token_out).unwrap() {
                again.push((job.token_in, job.token_out));
            }
        }
        let done: Vec<_> = first
            .iter()
            .map(|job| (job.token_in, job.token_out))
            .collect();
        catalog.finish_conduits(&done, &again, &[]).unwrap();
        assert!(again.is_empty());
        assert_eq!(super::enqueue_conduit_fill(&catalog).unwrap(), 0);
        assert!(catalog.claim_conduits(8, 1, 30).unwrap().is_empty());

        let stored = catalog.read_conduit(token_in, token_out).unwrap().unwrap();
        assert_eq!(stored.block, 10);
        assert_eq!(stored.generation, 1);
        assert_eq!(stored.members, 2);
        let mut pools: Vec<[u8; 32]> = stored
            .segments
            .iter()
            .map(|segment| segment.action.pool)
            .collect();
        pools.sort();
        pools.dedup();
        assert_eq!(pools.len(), 2);

        let merged_at = |input: &BigInt| quote(&stored.segments, input);
        for (detail, curve) in [(&deep, &deep_curve), (&thin, &thin_curve)] {
            let knots = direction_knots(detail, curve, token_in, token_out);
            let (sample_in, sample_out) = &knots[1];
            assert!(merged_at(sample_in) >= *sample_out);
            let amount = u256_from(&sample_in);
            let alone = estimate(curve, curve.generation, token_in, token_out, amount).unwrap();
            let alone_amount =
                BigInt::from_bytes_be(Sign::Plus, &alone.amount_out.to_be_bytes::<32>());
            assert_eq!(alone_amount, *sample_out);
        }
        let expected = expected_merge(&deep, &deep_curve, &thin, &thin_curve, token_in, token_out);
        let probe = BigInt::from(1_000);
        assert_eq!(merged_at(&probe), quote(&expected, &probe));

        let PoolDetail::ConstantProduct(pool) = &thin else {
            unreachable!();
        };
        let (body, mut write) = split_detail(&thin).unwrap();
        write.block = 12;
        catalog
            .put_immutable(&[ImmutableRow {
                kind: pool.kind,
                id: pool.id,
                body,
            }])
            .unwrap();
        super::commit_mutable(&catalog, &write).unwrap();
        let jobs = catalog.claim_conduits(8, 1, 30).unwrap();
        assert!(!jobs.is_empty());
        let mut again = Vec::new();
        for job in &jobs {
            if super::build_conduit(&catalog, job.token_in, job.token_out).unwrap() {
                again.push((job.token_in, job.token_out));
            }
        }
        let done: Vec<_> = jobs
            .iter()
            .map(|job| (job.token_in, job.token_out))
            .collect();
        catalog.finish_conduits(&done, &again, &[]).unwrap();
        let stored = catalog.read_conduit(token_in, token_out).unwrap().unwrap();
        assert_eq!(stored.members, 1);
        assert_eq!(stored.block, 10);
        assert!(stored
            .segments
            .iter()
            .all(|segment| segment.action.pool == deep_curve.id.0));
    }

    fn u256_from(value: &BigInt) -> U256 {
        let (_, bytes) = value.to_bytes_be();
        U256::from_be_slice(&bytes)
    }

    fn expected_merge(
        deep: &PoolDetail,
        deep_curve: &CurveRecord,
        thin: &PoolDetail,
        thin_curve: &CurveRecord,
        token_in: Address,
        token_out: Address,
    ) -> Vec<SlopeSegment> {
        let one = segments_from_knots(
            &direction_knots(deep, deep_curve, token_in, token_out),
            Action {
                kind: deep_curve.kind,
                pool: deep_curve.id.0,
            },
        );
        let two = segments_from_knots(
            &direction_knots(thin, thin_curve, token_in, token_out),
            Action {
                kind: thin_curve.kind,
                pool: thin_curve.id.0,
            },
        );
        routing::merge(&[one, two])
    }
}
