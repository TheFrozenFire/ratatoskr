//! Pool state, durable work, and the watch.
//!
//! Immutable rows are written once. Mutable rows, tick pages, and curves live
//! beside them in `entities.redb`. `work.redb` only remembers leases. A deleted
//! work file is rebuilt by scanning entities. The watch cursor and the dirty
//! flags commit together. A mutable snapshot clears its dirty flag in the same
//! transaction, and only when no newer event arrived during the read. A new
//! snapshot drops the curve.
//!
//! An empty snapshot stays in the catalog and gets no tick or curve job.
//! The curve is chosen from local quotes of the ingested snapshot: reserves,
//! tick words, balances, and the other fields that pool uses when it quotes.
//! Tick words still load when the bitmap lists them and those pages are not
//! stored, because the ladder's `max_in` is only known once the words are in.
//! Claim order is a rank index: a watch hit, then a larger `max_in`.
//! A curve write queues a pair conduit for each priced direction. That stage
//! merges after the curve batch, and it does not hold a curve lease.

use crate::catalog::{AERO, UNI_V4};
use crate::{
    Catalog, ChainStore, ConduitStore, RunOpts, RunReport, VenueRow, VenueStore, PANCAKE_V3,
    SUSHI_V3, UNI_V3, V3_FORK,
};
#[cfg(test)]
use alloy_primitives::Address;
use alloy_primitives::{B256, U256};
#[cfg(test)]
use chain::detail::BitmapWord;
use chain::detail::{tick_request, PoolDetail, TickWord, FLAG_FAILED};
use chain::rpc::Rpc;
use chain::tick::{self, TickWordRead};
use eyre::WrapErr;
#[cfg(test)]
use journal::Log;
use peer::HistoryPeer;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod codec;
mod drive;
mod empty;
mod follow;
mod plan;
mod refresh;
mod watch;

pub use empty::drop_empty_pools;

use self::codec::detail_flags;
pub use self::codec::{join_detail, split_detail};
#[cfg(test)]
use self::codec::{push_i32, push_u32, push_u64, push_u8};
pub use self::drive::run;
pub use self::follow::curve_for_snapshot;
pub(crate) use self::follow::keep_or_drop;
use self::follow::{after_book, follow_for_detail, FollowInput};
pub use self::follow::{plan_follow_up, Plan};
pub use self::refresh::{refresh_kinds, RefreshReport};
pub use self::watch::classify_log;
use self::watch::watch_once;

#[cfg(test)]
pub(crate) use store_model::decode_job;
pub(crate) use store_model::{priority_of, DIRTY_BAND};
pub use store_model::{
    CurveRecord, ImmutablePut, ImmutableRow, MutableStored, MutableWrite, PoolState, StateSummary,
    TickPage, WatchHit, WorkFail, WorkJob, CLASS_HEAD, CLASS_NONE, CLASS_TICKS, CURVE_VERSION,
    STAGE_CONDUIT, STAGE_CURVE, STAGE_IMMUTABLE, STAGE_MUTABLE, STAGE_TICKS, TICK_PAGE,
};

pub(crate) fn limits() -> config::Venues {
    config::Venues::default()
}

const TOPIC_V2_SYNC: B256 =
    alloy_primitives::b256!("1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1");
const TOPIC_V2_SWAP: B256 =
    alloy_primitives::b256!("d78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822");
const TOPIC_V3_SWAP: B256 =
    alloy_primitives::b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
const TOPIC_V3_MINT: B256 =
    alloy_primitives::b256!("7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde");
const TOPIC_V3_BURN: B256 =
    alloy_primitives::b256!("0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c");
const TOPIC_V4_SWAP: B256 =
    alloy_primitives::b256!("40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f");
const TOPIC_V4_LIQUIDITY: B256 =
    alloy_primitives::b256!("f208f4912782fd25c7f114ca3723a2d5dd6f3bcc3ac8db5af63baa85f711d5ec");
const TOPIC_BAL_SWAP: B256 =
    alloy_primitives::b256!("2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b");
const TOPIC_BAL_BALANCE: B256 =
    alloy_primitives::b256!("e5ce249087ce04f05a957192435400fd97868dba0e6a4b4c049abf8af80dae78");
const TOPIC_BAL3_SWAP: B256 =
    alloy_primitives::b256!("0874b2d545cb271cdbda4e093020c452328b24af12382ed62c4d00f5c26709db");
const TOPIC_PANCAKE_SWAP: B256 =
    alloy_primitives::b256!("19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83");
const TOPIC_CURVE_EXCHANGE: B256 =
    alloy_primitives::b256!("8b3e96f2b889fa771c53c981b40daf005f63f637f1869f707052d15a3dd97140");

pub(crate) fn curve_is_current(curve: Option<&CurveRecord>, generation: u64) -> bool {
    match curve {
        Some(curve) => curve.generation == generation && curve.version == CURVE_VERSION,
        None => false,
    }
}

pub fn derive_curve(detail: &PoolDetail, generation: u64) -> Result<CurveRecord, String> {
    let (kind, id) = detail_id(detail)?;
    Ok(CurveRecord {
        kind,
        id,
        generation,
        version: CURVE_VERSION,
        payload: crate::fit::curve_payload(detail),
    })
}

fn concentrated_kind(kind: u8) -> bool {
    matches!(
        kind,
        UNI_V3 | PANCAKE_V3 | SUSHI_V3 | V3_FORK | AERO | UNI_V4
    )
}

fn blank_job(stage: u8, kind: u8, id: B256) -> WorkJob {
    WorkJob {
        stage,
        kind,
        id,
        lease_until: 0,
        attempt: 0,
        not_before: 0,
        generation: 0,
        tick_cursor: 0,
        class: CLASS_NONE,
        priority: 0,
        error: String::new(),
    }
}

pub(crate) fn curve_job(kind: u8, id: B256, generation: u64) -> WorkJob {
    let mut job = blank_job(STAGE_CURVE, kind, id);
    job.generation = generation;
    job
}

fn job_for(jobs: &[WorkJob], kind: u8, id: B256) -> Option<&WorkJob> {
    jobs.iter().find(|job| job.kind == kind && job.id == id)
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn unpack_stored(word: U256) -> (u128, i128) {
    let gross = (word & U256::from(u128::MAX)).to::<u128>();
    let high: U256 = word >> 128;
    let net = high.to::<u128>() as i128;
    (gross, net)
}

fn detail_id(detail: &PoolDetail) -> Result<(u8, B256), String> {
    Ok(match detail {
        PoolDetail::ConstantProduct(pool) => (pool.kind, pool.id),
        PoolDetail::Concentrated(pool) => (pool.kind, pool.id),
        PoolDetail::Balancer(pool) => (pool.kind, pool.id),
        PoolDetail::Curve(pool) => (pool.kind, pool.id),
        PoolDetail::Metric(pool) => (pool.kind, pool.id),
        PoolDetail::Failed(_) => return Err("pool read failed".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Put;
    use alloy_primitives::address;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use store_model::UNI_V2;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-state-{}-{}-{}",
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

    fn reconcile(catalog: &Catalog, now: u64) -> eyre::Result<u64> {
        super::plan::reconcile_catalog(catalog, now)
    }

    fn open() -> (TempDir, Catalog) {
        let dir = TempDir::new();
        crate::install_catalog_hooks();
        let catalog = Catalog::open(&dir.0).unwrap();
        (dir, catalog)
    }

    #[test]
    fn a_summary_counts_the_stored_identities() {
        let (_dir, catalog) = open();
        catalog
            .commit_chunk("lists", 2, &[row(1), row(2)], crate::Put::IfAbsent)
            .unwrap();
        let summary = catalog.state_summary().unwrap();
        assert_eq!(summary.identities, 2);
        assert_eq!(summary.immutable, 0);
        assert_eq!(summary.curves, 0);
        assert_eq!(summary.jobs, 0);
        assert_eq!(catalog.token_count().unwrap(), 0);
    }

    fn row(byte: u8) -> VenueRow {
        let mut id = B256::ZERO;
        id[31] = byte;
        VenueRow {
            kind: UNI_V2,
            id,
            factory: address!("2222222222222222222222222222222222222222"),
            tokens: vec![
                address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            ],
            fee: Some(3000),
            tick_spacing: None,
            block_number: Some(10),
            extra: None,
            block_hash: None,
            tx_index: None,
            log_index: None,
        }
    }

    fn product() -> PoolDetail {
        PoolDetail::ConstantProduct(chain::detail::ConstantProduct {
            kind: UNI_V2,
            id: row(1).id,
            pair: address!("1111111111111111111111111111111111111111"),
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            reserve0: U256::from(1_000_000u64),
            reserve1: U256::from(1_000_000u64),
            fee_pips: Some(3000),
            timestamp: 4,
            flags: 0,
        })
    }

    #[test]
    fn drop_empty_removes_a_zero_reserve_and_keeps_a_one_wei_pool() {
        let (_dir, catalog) = open();
        let mut dead = product();
        let PoolDetail::ConstantProduct(ref mut pool) = dead else {
            unreachable!();
        };
        pool.id = row(1).id;
        pool.reserve0 = U256::ZERO;
        pool.reserve1 = U256::ZERO;
        let mut small = product();
        let PoolDetail::ConstantProduct(ref mut pool) = small else {
            unreachable!();
        };
        pool.id = row(2).id;
        pool.reserve0 = U256::from(1u64);
        pool.reserve1 = U256::from(1u64);
        store_detail(&catalog, &row(1), &dead);
        store_detail(&catalog, &row(2), &small);
        catalog
            .commit_chunk("list", 1, &[row(3)], Put::Merge)
            .unwrap();
        assert_eq!(super::drop_empty_pools(&catalog).unwrap().len(), 1);
        assert!(catalog.read_venue(UNI_V2, row(1).id).unwrap().is_none());
        assert!(catalog.read_venue(UNI_V2, row(2).id).unwrap().is_some());
        assert!(catalog.read_venue(UNI_V2, row(3).id).unwrap().is_some());
    }

    fn store_detail(catalog: &Catalog, venue: &VenueRow, detail: &PoolDetail) {
        catalog
            .commit_chunk("list", 1, std::slice::from_ref(venue), Put::Merge)
            .unwrap();
        let (body, mut write) = split_detail(detail).unwrap();
        write.block = 10;
        catalog
            .put_immutable(&[ImmutableRow {
                kind: venue.kind,
                id: venue.id,
                body,
            }])
            .unwrap();
        crate::conduit::commit_mutable(&catalog, &write).unwrap();
    }

    #[test]
    fn deleting_work_rebuilds_the_immutable_job() {
        let (dir, catalog) = open();
        catalog
            .commit_chunk("list", 1, &[row(1)], Put::Merge)
            .unwrap();
        assert_eq!(reconcile(&catalog, 1).unwrap(), 1);
        let claimed = catalog.claim_work(STAGE_IMMUTABLE, 16, 1, 10).unwrap();
        assert_eq!(claimed.len(), 1);
        assert!(catalog
            .claim_work(STAGE_MUTABLE, 16, 1, 10)
            .unwrap()
            .is_empty());
        drop(catalog);
        std::fs::remove_file(dir.0.join("work.redb")).unwrap();
        let catalog = Catalog::open(&dir.0).unwrap();
        assert!(catalog
            .claim_work(STAGE_IMMUTABLE, 16, 1, 10)
            .unwrap()
            .is_empty());
        assert_eq!(reconcile(&catalog, 2).unwrap(), 1);
        assert_eq!(
            catalog
                .claim_work(STAGE_IMMUTABLE, 16, 2, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn immutable_is_write_once_and_a_mismatch_is_kept() {
        let (_dir, catalog) = open();
        let first = ImmutableRow {
            kind: UNI_V2,
            id: row(1).id,
            body: b"one".to_vec(),
        };
        assert_eq!(
            catalog.put_immutable(std::slice::from_ref(&first)).unwrap(),
            vec![ImmutablePut::Written]
        );
        assert_eq!(
            catalog.put_immutable(&[first]).unwrap(),
            vec![ImmutablePut::Present]
        );
        let second = ImmutableRow {
            kind: UNI_V2,
            id: row(1).id,
            body: b"two".to_vec(),
        };
        assert_eq!(
            catalog.put_immutable(&[second]).unwrap(),
            vec![ImmutablePut::Mismatch]
        );
        assert_eq!(catalog.immutable_mismatch(UNI_V2, row(1).id).unwrap(), 1);
        let state = catalog.read_state(UNI_V2, row(1).id).unwrap();
        assert_eq!(state.immutable.unwrap(), b"one");
    }

    #[test]
    fn watch_cursor_does_not_pass_an_unrecorded_hit() {
        let (_dir, catalog) = open();
        let hit = WatchHit {
            kind: UNI_V2,
            id: row(1).id,
            block: 5,
            class: CLASS_HEAD,
        };
        assert!(catalog.apply_watch(5, std::slice::from_ref(&hit)).is_err());
        assert!(catalog.watch_through().unwrap().is_none());
        catalog.apply_watch(6, &[hit]).unwrap();
        assert_eq!(catalog.watch_through().unwrap(), Some(6));
        let err = catalog.apply_watch(4, &[]);
        assert!(err.is_err(), "{err:?}");
        assert_eq!(catalog.watch_through().unwrap(), Some(6));
    }

    #[test]
    fn dirty_clear_is_conditional_and_ticks_do_not_downgrade() {
        let (_dir, catalog) = open();
        let id = row(1).id;
        let write = MutableWrite {
            kind: UNI_V2,
            id,
            block: 10,
            class: CLASS_NONE,
            bitmap: B256::ZERO,
            tick_count: 0,
            truncated: false,
            next_scan: None,
            body: b"m".to_vec(),
        };
        crate::conduit::commit_mutable(&catalog, &write).unwrap();
        catalog
            .apply_watch(
                20,
                &[
                    WatchHit {
                        kind: UNI_V2,
                        id,
                        block: 12,
                        class: CLASS_HEAD,
                    },
                    WatchHit {
                        kind: UNI_V2,
                        id,
                        block: 14,
                        class: CLASS_TICKS,
                    },
                    WatchHit {
                        kind: UNI_V2,
                        id,
                        block: 13,
                        class: CLASS_HEAD,
                    },
                ],
            )
            .unwrap();
        let dirty = catalog.read_state(UNI_V2, id).unwrap().dirty.unwrap();
        assert_eq!(dirty.block, 14);
        assert_eq!(dirty.class, CLASS_TICKS);
        let mut older = write.clone();
        older.block = 13;
        older.class = CLASS_HEAD;
        let stored = crate::conduit::commit_mutable(&catalog, &older).unwrap();
        assert!(!stored.dirty_cleared);
        assert_eq!(
            catalog.read_state(UNI_V2, id).unwrap().dirty.unwrap().class,
            CLASS_TICKS
        );
        let mut caught = write;
        caught.block = 14;
        caught.class = CLASS_TICKS;
        let stored = crate::conduit::commit_mutable(&catalog, &caught).unwrap();
        assert!(stored.dirty_cleared);
        assert!(catalog.read_state(UNI_V2, id).unwrap().dirty.is_none());
    }

    #[test]
    fn head_keeps_ticks_when_the_bitmap_matches_and_a_mint_drops_them() {
        let (_dir, catalog) = open();
        let id = row(7).id;
        let bitmap = B256::repeat_byte(1);
        let mut write = MutableWrite {
            kind: UNI_V3,
            id,
            block: 3,
            class: CLASS_NONE,
            bitmap,
            tick_count: 1,
            truncated: false,
            next_scan: None,
            body: b"bitmap".to_vec(),
        };
        let first = crate::conduit::commit_mutable(&catalog, &write).unwrap();
        catalog
            .put_tick_pages(&[TickPage {
                kind: UNI_V3,
                id,
                page: 0,
                generation: first.generation,
                ticks: vec![TickWord {
                    tick: 60,
                    liquidity_gross: 5,
                    liquidity_net: 5,
                }],
            }])
            .unwrap();
        write.block = 4;
        write.class = CLASS_HEAD;
        let head = crate::conduit::commit_mutable(&catalog, &write).unwrap();
        assert!(head.kept_ticks);
        assert!(!head.bitmap_changed);
        let state = catalog.read_state(UNI_V3, id).unwrap();
        assert_eq!(state.ticks.len(), 1);
        assert_eq!(state.mutable.unwrap().generation, head.generation);
        write.class = CLASS_TICKS;
        write.block = 5;
        let minted = crate::conduit::commit_mutable(&catalog, &write).unwrap();
        assert!(!minted.kept_ticks);
        assert!(catalog.read_state(UNI_V3, id).unwrap().ticks.is_empty());
        write.bitmap = B256::repeat_byte(2);
        write.class = CLASS_HEAD;
        write.block = 6;
        catalog
            .put_tick_pages(&[TickPage {
                kind: UNI_V3,
                id,
                page: 0,
                generation: minted.generation,
                ticks: vec![TickWord {
                    tick: 1,
                    liquidity_gross: 1,
                    liquidity_net: 1,
                }],
            }])
            .unwrap();
        let changed = crate::conduit::commit_mutable(&catalog, &write).unwrap();
        assert!(changed.bitmap_changed);
        assert!(catalog.read_state(UNI_V3, id).unwrap().ticks.is_empty());
    }

    #[test]
    fn a_head_refresh_leaves_tick_pages_at_their_book_generation() {
        let (_dir, catalog) = open();
        let id = row(7).id;
        let bitmap = B256::repeat_byte(1);
        let mut write = MutableWrite {
            kind: UNI_V3,
            id,
            block: 3,
            class: CLASS_NONE,
            bitmap,
            tick_count: 1,
            truncated: false,
            next_scan: None,
            body: b"bitmap".to_vec(),
        };
        let first = crate::conduit::commit_mutable(&catalog, &write).unwrap();
        catalog
            .put_tick_pages(&[TickPage {
                kind: UNI_V3,
                id,
                page: 0,
                generation: first.generation,
                ticks: vec![TickWord {
                    tick: 60,
                    liquidity_gross: 5,
                    liquidity_net: 5,
                }],
            }])
            .unwrap();
        write.block = 4;
        write.class = CLASS_HEAD;
        let head = crate::conduit::commit_mutable(&catalog, &write).unwrap();
        assert!(head.kept_ticks);
        let view = catalog.read_state(UNI_V3, id).unwrap().mutable.unwrap();
        assert_eq!(view.book, first.generation);
        assert_ne!(view.generation, view.book);
        assert_eq!(
            catalog
                .first_missing_tick_page(UNI_V3, id, view.book, view.tick_count)
                .unwrap(),
            1
        );
        assert_eq!(
            catalog
                .first_missing_tick_page(UNI_V3, id, view.generation, view.tick_count)
                .unwrap(),
            0
        );
        assert!(catalog
            .put_tick_pages(&[TickPage {
                kind: UNI_V3,
                id,
                page: 1,
                generation: view.generation,
                ticks: Vec::new(),
            }])
            .is_err());
    }

    #[test]
    fn a_stale_curve_is_rejected_and_reconcile_queues_another() {
        let (_dir, catalog) = open();
        let sample = row(1);
        catalog
            .commit_chunk("list", 1, std::slice::from_ref(&sample), Put::Merge)
            .unwrap();
        let (immutable, mut mutable) = split_detail(&product()).unwrap();
        mutable.block = 8;
        catalog
            .put_immutable(&[ImmutableRow {
                kind: sample.kind,
                id: sample.id,
                body: immutable,
            }])
            .unwrap();
        let stored = crate::conduit::commit_mutable(&catalog, &mutable).unwrap();
        let detail = product();
        let mut curve = derive_curve(&detail, stored.generation).unwrap();
        curve.generation = stored.generation + 1;
        assert!(crate::conduit::commit_curve(&catalog, &curve).is_err());
        curve.generation = stored.generation;
        crate::conduit::commit_curve(&catalog, &curve).unwrap();
        let quote = crate::estimate(
            &curve,
            stored.generation,
            address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            U256::from(1_000u64),
        )
        .unwrap();
        let direct = detail
            .quote(
                address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
                address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
                U256::from(1_000u64),
            )
            .unwrap();
        assert_eq!(quote, direct);
        assert_eq!(reconcile(&catalog, 1).unwrap(), 0);
        mutable.block = 9;
        mutable.class = CLASS_HEAD;
        crate::conduit::commit_mutable(&catalog, &mutable).unwrap();
        let kept = catalog.read_state(sample.kind, sample.id).unwrap();
        assert!(kept.curve.is_none());
        assert!(reconcile(&catalog, 1).unwrap() >= 1);
        assert_eq!(catalog.claim_work(STAGE_CURVE, 4, 1, 10).unwrap().len(), 1);
    }

    #[test]
    fn reconcile_does_not_reread_a_complete_tick_page() {
        let (_dir, catalog) = open();
        let mut sample = row(9);
        sample.kind = UNI_V3;
        catalog
            .commit_chunk("list", 1, std::slice::from_ref(&sample), Put::Merge)
            .unwrap();
        catalog
            .put_immutable(&[ImmutableRow {
                kind: UNI_V3,
                id: sample.id,
                body: b"i".to_vec(),
            }])
            .unwrap();
        let write = MutableWrite {
            kind: UNI_V3,
            id: sample.id,
            block: 4,
            class: CLASS_NONE,
            bitmap: B256::ZERO,
            tick_count: 1,
            truncated: false,
            next_scan: None,
            body: b"m".to_vec(),
        };
        let stored = crate::conduit::commit_mutable(&catalog, &write).unwrap();
        assert_eq!(reconcile(&catalog, 1).unwrap(), 1);
        assert_eq!(catalog.claim_work(STAGE_TICKS, 4, 1, 10).unwrap().len(), 1);
        catalog
            .put_tick_pages(&[TickPage {
                kind: UNI_V3,
                id: sample.id,
                page: 0,
                generation: stored.generation,
                ticks: vec![TickWord {
                    tick: 60,
                    liquidity_gross: 1,
                    liquidity_net: 1,
                }],
            }])
            .unwrap();
        catalog
            .finish_work(&[(STAGE_TICKS, UNI_V3, sample.id)], &[])
            .unwrap();
        assert_eq!(reconcile(&catalog, 2).unwrap(), 1);
        assert!(catalog
            .claim_work(STAGE_TICKS, 4, 2, 10)
            .unwrap()
            .is_empty());
        assert_eq!(catalog.claim_work(STAGE_CURVE, 4, 2, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_stored_product_curve_uses_the_row_kind() {
        let (_dir, catalog) = open();
        let sample = row(3);
        let mut pancake = product();
        let PoolDetail::ConstantProduct(pool) = &mut pancake else {
            panic!("product");
        };
        pool.kind = store_model::PANCAKE_V2;
        pool.id = sample.id;
        pool.fee_pips = None;
        let (immutable, mut mutable) = split_detail(&pancake).unwrap();
        assert!(!immutable
            .windows(32)
            .any(|word| word == sample.id.as_slice()));
        catalog
            .put_immutable(&[ImmutableRow {
                kind: store_model::PANCAKE_V2,
                id: sample.id,
                body: immutable.clone(),
            }])
            .unwrap();
        mutable.kind = store_model::PANCAKE_V2;
        mutable.id = sample.id;
        mutable.block = 8;
        let stored = crate::conduit::commit_mutable(&catalog, &mutable).unwrap();
        let joined = join_detail(&immutable, &mutable.body, &[], mutable.kind, mutable.id).unwrap();
        let PoolDetail::ConstantProduct(read) = &joined else {
            panic!("joined");
        };
        assert_eq!(read.kind, store_model::PANCAKE_V2);
        assert_eq!(read.id, sample.id);
        let curve = derive_curve(&joined, stored.generation).unwrap();
        assert_eq!(curve.kind, store_model::PANCAKE_V2);
        assert_eq!(curve.id, sample.id);
        crate::conduit::commit_curve(&catalog, &curve).unwrap();
        let amount = U256::from(10_000_000u64);
        let token_in = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token_out = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let quoted =
            crate::estimate(&curve, stored.generation, token_in, token_out, amount).unwrap();
        let direct = joined.quote(token_in, token_out, amount).unwrap();
        assert_eq!(quoted, direct);
        let mut unstamped = pancake;
        let PoolDetail::ConstantProduct(pool) = &mut unstamped else {
            panic!("product");
        };
        pool.kind = 0;
        let wrong = unstamped.quote(token_in, token_out, amount).unwrap();
        assert_ne!(quoted.amount_out, wrong.amount_out);
    }

    #[test]
    fn plan_isolates_head_from_ticks() {
        let head: Plan = plan_follow_up(CLASS_HEAD, false, false, 10, true);
        assert_eq!(head.stage, Some(STAGE_CURVE));
        let changed = plan_follow_up(CLASS_HEAD, true, false, 10, true);
        assert_eq!(changed.stage, Some(STAGE_TICKS));
        let mint = plan_follow_up(CLASS_TICKS, false, false, 10, true);
        assert_eq!(mint.stage, Some(STAGE_TICKS));
        let truncated = plan_follow_up(CLASS_HEAD, true, true, 10, true);
        assert!(truncated.retry_mutable);
        assert!(truncated.stage.is_none());
    }

    #[test]
    fn classify_keeps_factory_logs_and_marks_mint_as_ticks() {
        let pool = address!("1111111111111111111111111111111111111111");
        let id = VenueRow::address_id(pool);
        let has = |kind: u8, found: B256| kind == UNI_V3 && found == id;
        let address = |emitter: Address| {
            if emitter == pool {
                Some((UNI_V3, id))
            } else {
                None
            }
        };
        let mint = Log {
            block_number: 9,
            block_hash: B256::ZERO,
            tx_index: 0,
            tx_hash: None,
            log_index: 1,
            address: pool,
            topics: vec![TOPIC_V3_MINT],
            data: alloy_primitives::Bytes::new(),
        };
        let hit = classify_log(&mint, has, address).unwrap();
        assert_eq!(hit.class, CLASS_TICKS);
        assert_eq!(hit.id, id);
        let factory = Log {
            address: chain::decode::known().uni_v3,
            topics: vec![TOPIC_V3_SWAP],
            ..mint.clone()
        };
        assert!(classify_log(&factory, has, address).is_none());
    }

    #[test]
    fn an_uninitialized_pool_stores_a_skipped_curve() {
        let detail = PoolDetail::Concentrated(chain::detail::Concentrated {
            kind: UNI_V3,
            id: row(1).id,
            home: Address::ZERO,
            hooks: Address::ZERO,
            token0: Address::ZERO,
            token1: Address::ZERO,
            fee_pips: Some(3000),
            protocol_fee: 0,
            tick_spacing: 60,
            sqrt_price_x96: U256::ZERO,
            tick: 0,
            liquidity: 0,
            next_scan: None,
            bitmaps: Vec::new(),
            ticks: Vec::new(),
            flags: 0,
        });
        let curve = derive_curve(&detail, 3).unwrap();
        assert_eq!(curve.kind, UNI_V3);
        assert_eq!(curve.id, row(1).id);
        assert_eq!(curve.payload, vec![1, 5, 0, 0, 0, 0]);
    }

    fn with_reserves(id: u8, reserve: u64) -> (VenueRow, PoolDetail) {
        let venue = row(id);
        let mut detail = product();
        let PoolDetail::ConstantProduct(pool) = &mut detail else {
            unreachable!();
        };
        pool.id = venue.id;
        pool.reserve0 = U256::from(reserve);
        pool.reserve1 = U256::from(reserve);
        (venue, detail)
    }

    #[test]
    fn priority_orders_dirty_ahead_of_a_larger_pool() {
        assert!(priority_of(false, U256::from(1u64)) < priority_of(false, U256::from(10_000u64)));
        assert!(
            priority_of(false, U256::from(10_000u64)) < priority_of(false, U256::from(u64::MAX))
        );
        assert!(priority_of(false, U256::from(u64::MAX)) < priority_of(true, U256::from(1u64)));
    }

    #[test]
    fn a_tag1_job_decodes_with_priority_zero() {
        let mut key = [0u8; 34];
        key[0] = STAGE_CURVE;
        key[1] = UNI_V2;
        let mut bytes = vec![1];
        push_u64(&mut bytes, 0);
        push_u32(&mut bytes, 1);
        push_u64(&mut bytes, 0);
        push_u64(&mut bytes, 4);
        push_i32(&mut bytes, 0);
        push_u8(&mut bytes, 0);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        let job = decode_job(&key, &bytes).unwrap();
        assert_eq!(job.priority, 0);
        assert_eq!(job.attempt, 1);
        assert_eq!(job.generation, 4);
        assert_eq!(job.stage, STAGE_CURVE);
    }

    #[test]
    fn an_empty_snapshot_drops_follow_up_and_keeps_the_identity() {
        let (_dir, catalog) = open();
        let (venue, detail) = with_reserves(3, 0);
        store_detail(&catalog, &venue, &detail);
        let mut seeded = curve_job(venue.kind, venue.id, 1);
        seeded.priority = 50;
        catalog
            .enqueue_work(std::slice::from_ref(&seeded), 1)
            .unwrap();
        reconcile(&catalog, 1).unwrap();
        assert!(catalog
            .claim_work(STAGE_TICKS, 4, 1, 10)
            .unwrap()
            .is_empty());
        assert!(catalog
            .claim_work(STAGE_CURVE, 4, 1, 10)
            .unwrap()
            .is_empty());
        assert!(catalog.read_venue(venue.kind, venue.id).unwrap().is_some());
    }

    #[test]
    fn a_one_wei_pool_derives_locally() {
        let (_dir, catalog) = open();
        let (venue, detail) = with_reserves(2, 1);
        store_detail(&catalog, &venue, &detail);
        reconcile(&catalog, 1).unwrap();
        let curves = catalog.claim_work(STAGE_CURVE, 4, 1, 10).unwrap();
        assert_eq!(curves.len(), 1);
        assert!(curves[0].priority > 0);
        assert!(curves[0].priority < DIRTY_BAND);
        assert!(catalog.read_venue(venue.kind, venue.id).unwrap().is_some());
        let plan = follow_for_detail(&FollowInput {
            kind: venue.kind,
            id: venue.id,
            detail: &detail,
            class: CLASS_NONE,
            bitmap_changed: false,
            truncated: false,
            tick_count: 0,
            generation: 1,
            watched: false,
            book_ready: true,
        });
        assert!(plan.job.is_none());
        assert!(plan.derive);
    }

    #[test]
    fn a_larger_pool_is_claimed_before_a_smaller_one() {
        let (_dir, catalog) = open();
        let (small, small_detail) = with_reserves(1, 200);
        let (big, big_detail) = with_reserves(9, 2_000_000);
        store_detail(&catalog, &small, &small_detail);
        store_detail(&catalog, &big, &big_detail);
        reconcile(&catalog, 1).unwrap();
        let first = catalog.claim_work(STAGE_CURVE, 1, 1, 10).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, big.id);
        let second = catalog.claim_work(STAGE_CURVE, 1, 1, 10).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, small.id);
        assert!(first[0].priority > second[0].priority);
        assert!(first[0].priority < DIRTY_BAND);
    }

    #[test]
    fn a_dirty_job_is_claimed_before_a_large_quiet_pool() {
        let (_dir, catalog) = open();
        let mut quiet = curve_job(UNI_V2, row(1).id, 1);
        quiet.priority = priority_of(false, U256::from(u64::MAX));
        let mut dirty = curve_job(UNI_V2, row(9).id, 1);
        dirty.priority = priority_of(true, U256::from(1u64));
        catalog.enqueue_work(&[quiet, dirty], 1).unwrap();
        let claimed = catalog.claim_work(STAGE_CURVE, 1, 1, 10).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, row(9).id);
        assert!(claimed[0].priority >= DIRTY_BAND);
    }

    #[test]
    fn a_leased_job_is_not_replaced() {
        let (_dir, catalog) = open();
        let mut job = curve_job(UNI_V2, row(1).id, 1);
        job.priority = 1;
        catalog.enqueue_work(std::slice::from_ref(&job), 1).unwrap();
        assert_eq!(catalog.claim_work(STAGE_CURVE, 1, 1, 100).unwrap().len(), 1);
        let mut hotter = job;
        hotter.priority = 99;
        hotter.lease_until = 0;
        catalog
            .enqueue_work(std::slice::from_ref(&hotter), 1)
            .unwrap();
        assert!(catalog
            .claim_work(STAGE_CURVE, 1, 1, 10)
            .unwrap()
            .is_empty());
        let later = catalog.claim_work(STAGE_CURVE, 1, 101, 10).unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].priority, 1);
    }

    #[test]
    fn a_watch_hit_requeues_the_mutable_refresh_first() {
        let (_dir, catalog) = open();
        let (venue, detail) = with_reserves(5, 1_000_000);
        store_detail(&catalog, &venue, &detail);
        catalog
            .apply_watch(
                12,
                &[WatchHit {
                    kind: venue.kind,
                    id: venue.id,
                    block: 11,
                    class: CLASS_HEAD,
                }],
            )
            .unwrap();
        let jobs = catalog
            .claim_work(STAGE_MUTABLE, 4, unix_now(), 10)
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, venue.id);
        assert!(jobs[0].priority >= DIRTY_BAND);
    }

    #[test]
    fn an_unmeasured_bitmap_still_queues_ticks() {
        let (_dir, catalog) = open();
        let mut venue = row(4);
        venue.kind = UNI_V3;
        let detail = PoolDetail::Concentrated(chain::detail::Concentrated {
            kind: UNI_V3,
            id: venue.id,
            home: Address::ZERO,
            hooks: Address::ZERO,
            token0: address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            token1: address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            fee_pips: Some(3000),
            protocol_fee: 0,
            tick_spacing: 60,
            sqrt_price_x96: U256::from(1u64),
            tick: 0,
            liquidity: 0,
            next_scan: None,
            bitmaps: vec![BitmapWord {
                word_pos: 0,
                bits: U256::from(1u64),
            }],
            ticks: Vec::new(),
            flags: 0,
        });
        store_detail(&catalog, &venue, &detail);
        reconcile(&catalog, 1).unwrap();
        assert_eq!(catalog.claim_work(STAGE_TICKS, 4, 1, 10).unwrap().len(), 1);
    }

    #[test]
    fn a_full_sample_is_a_quote_and_dust_is_local() {
        let (_dir, _catalog) = open();
        let (venue, detail) = with_reserves(1, 1_000_000);
        let quote = follow_for_detail(&FollowInput {
            kind: venue.kind,
            id: venue.id,
            detail: &detail,
            class: CLASS_NONE,
            bitmap_changed: false,
            truncated: false,
            tick_count: 0,
            generation: 3,
            watched: false,
            book_ready: true,
        });
        assert!(quote.job.is_none());
        assert!(quote.derive);
        assert!(quote.priority < DIRTY_BAND);
        let (venue, detail) = with_reserves(1, 0);
        let empty = follow_for_detail(&FollowInput {
            kind: venue.kind,
            id: venue.id,
            detail: &detail,
            class: CLASS_NONE,
            bitmap_changed: false,
            truncated: false,
            tick_count: 0,
            generation: 3,
            watched: true,
            book_ready: true,
        });
        assert!(empty.job.is_none());
        assert!(!empty.derive);
    }
}
