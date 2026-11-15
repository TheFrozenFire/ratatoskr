//! Pool state, durable work, and the watch.
//!
//! Immutable rows are written once. Mutable rows, tick pages, on-chain quote
//! samples, and curves live beside them in `entities.redb`. `work.redb` only
//! remembers leases. A deleted work file is rebuilt by scanning entities. The
//! watch cursor and the dirty flags commit together. A mutable snapshot clears
//! its dirty flag in the same transaction, and only when no newer event arrived
//! during the read. A new snapshot drops the curve. Stored quote samples stay.
//!
//! An empty snapshot stays in the catalog and gets no tick or curve job.
//! The state pipeline does not call the quoter. Chain samples already stored
//! stay in the quote table for validation. A new snapshot does not delete
//! them, and a curve is never fit to them. The curve is chosen from local
//! quotes of the ingested snapshot: reserves, tick words, balances, and the
//! other fields that pool uses when it quotes.
//! Tick words still load when the bitmap lists them and those pages are not
//! stored, because the ladder's `max_in` is only known once the words are in.
//! Claim order is a rank index: a watch hit, then a larger `max_in`.
//! A curve write queues a pair conduit for each priced direction. That stage
//! merges after the curve batch, and it does not hold a curve lease.

use crate::catalog::{AERO, BALANCER_V2, BALANCER_V3, CURVE, METRIC, UNI_V4};
use crate::detail::{
    tick_request, BitmapWord, PoolDetail, Quote, TickWord, FLAG_CRYPTO, FLAG_FAILED, FLAG_STABLE,
    FLAG_TRUNCATED,
};
use crate::rpc::Rpc;
use crate::tick::{self, TickWordRead};
use crate::{
    Catalog, ChainStore, ConduitStore, RunOpts, RunReport, VenueRow, VenueStore, PANCAKE_V3,
    SUSHI_V3, UNI_V3, V3_FORK,
};
use alloy_primitives::{keccak256, Address, B256, U256};
use eyre::WrapErr;
use ipc::HistoryPeer;
use journal::Log;
use num_bigint::{BigInt, Sign};
use num_traits::Zero;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use store_model::BlockRecord;

mod codec;
mod drive;

use self::codec::*;
pub use self::drive::run;

#[cfg(test)]
pub(crate) use store_model::decode_job;
pub(crate) use store_model::{priority_of, DIRTY_BAND};
pub use store_model::{
    CurveRecord, ImmutablePut, ImmutableRow, MutableStored, MutableView, MutableWrite, PoolState,
    QuoteRecord, StateSummary, TickPage, WatchHit, WorkFail, WorkJob, CLASS_HEAD, CLASS_NONE,
    CLASS_TICKS, CURVE_VERSION, QUOTE_VERSION, STAGE_CONDUIT, STAGE_CURVE, STAGE_IMMUTABLE,
    STAGE_MUTABLE, STAGE_QUOTE, STAGE_TICKS, TICK_PAGE,
};

const DETAIL_BATCH: u32 = 16;
const LEASE_SECS: u64 = 120;
const RETRY_SECS: u64 = 30;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub retry_mutable: bool,
    pub stage: Option<u8>,
}

/// What to enqueue after a mutable snapshot commits.
///
/// A head refresh with the same bitmap keeps tick pages and derives a curve.
/// A mint or a changed bitmap has to read ticks again. A truncated bitmap
/// stays on the mutable stage. The quoter is not a stage.
pub fn plan_follow_up(
    class: u8,
    bitmap_changed: bool,
    truncated: bool,
    tick_count: u32,
    _canonical: bool,
) -> Plan {
    if truncated {
        return Plan {
            retry_mutable: true,
            stage: None,
        };
    }
    let reread = tick_count > 0 && (bitmap_changed || class == CLASS_TICKS || class == CLASS_NONE);
    if reread {
        return Plan {
            retry_mutable: false,
            stage: Some(STAGE_TICKS),
        };
    }
    Plan {
        retry_mutable: false,
        stage: Some(STAGE_CURVE),
    }
}

/// Stored chain samples match this snapshot and the current sampling scheme.
pub fn quote_is_current(quote: Option<&QuoteRecord>, mutable: &MutableView) -> bool {
    match quote {
        Some(quote) => {
            quote.generation == mutable.generation
                && quote.version == QUOTE_VERSION
                && quote.block == mutable.block
        }
        None => false,
    }
}

pub(crate) fn curve_is_current(curve: Option<&CurveRecord>, generation: u64) -> bool {
    match curve {
        Some(curve) => curve.generation == generation && curve.version == CURVE_VERSION,
        None => false,
    }
}

pub fn split_detail(detail: &PoolDetail) -> Result<(Vec<u8>, MutableWrite), String> {
    if detail_flags(detail) & FLAG_FAILED != 0 {
        return Err("pool read failed".into());
    }
    let (kind, id, bitmap, tick_count, truncated, next_scan, immutable, body) = match detail {
        PoolDetail::ConstantProduct(pool) => (
            pool.kind,
            pool.id,
            B256::ZERO,
            0,
            false,
            None,
            encode_product_immutable(pool),
            encode_product_mutable(pool),
        ),
        PoolDetail::Concentrated(pool) => (
            pool.kind,
            pool.id,
            bitmap_fingerprint(&pool.bitmaps),
            u32::try_from(pool.initialized_ticks().len()).unwrap_or(u32::MAX),
            pool.flags & FLAG_TRUNCATED != 0,
            pool.next_scan,
            encode_concentrated_immutable(pool),
            encode_concentrated_mutable(pool),
        ),
        PoolDetail::Balancer(pool) => (
            pool.kind,
            pool.id,
            B256::ZERO,
            0,
            false,
            None,
            encode_balancer_immutable(pool),
            encode_balancer_mutable(pool),
        ),
        PoolDetail::Curve(pool) => (
            pool.kind,
            pool.id,
            B256::ZERO,
            0,
            false,
            None,
            encode_curve_immutable(pool),
            encode_curve_mutable(pool),
        ),
        PoolDetail::Metric(pool) => (
            pool.kind,
            pool.id,
            B256::ZERO,
            0,
            pool.flags & FLAG_TRUNCATED != 0,
            pool.next_scan,
            encode_metric_immutable(pool),
            encode_metric_mutable(pool),
        ),
        PoolDetail::Failed(_) => return Err("pool read failed".into()),
    };
    Ok((
        immutable,
        MutableWrite {
            kind,
            id,
            block: 0,
            class: CLASS_NONE,
            bitmap,
            tick_count,
            truncated,
            next_scan,
            body,
        },
    ))
}

/// Rebuild a pool from stored rows.
///
/// Product immutables do not carry kind or id. `kind` and `id` are the entity
/// key, and they are stamped onto the product so quotes and curves use it.
pub fn join_detail(
    immutable: &[u8],
    mutable_body: &[u8],
    ticks: &[TickWord],
    kind: u8,
    id: B256,
) -> Result<PoolDetail, String> {
    if immutable.is_empty() {
        return Err("immutable".into());
    }
    match immutable[0] {
        1 => {
            let mut pool = decode_product(immutable, mutable_body)?;
            pool.kind = kind;
            pool.id = id;
            Ok(PoolDetail::ConstantProduct(pool))
        }
        2 => {
            let mut pool = decode_concentrated(immutable, mutable_body)?;
            pool.ticks = ticks.to_vec();
            pool.ticks.sort_by_key(|tick| tick.tick);
            Ok(PoolDetail::Concentrated(pool))
        }
        3 => Ok(PoolDetail::Balancer(decode_balancer(
            immutable,
            mutable_body,
        )?)),
        4 => Ok(PoolDetail::Curve(decode_curve(immutable, mutable_body)?)),
        5 => Ok(PoolDetail::Metric(decode_metric(immutable, mutable_body)?)),
        _ => Err("detail tag".into()),
    }
}

pub fn is_canonical(detail: &PoolDetail) -> bool {
    match detail {
        PoolDetail::ConstantProduct(_) | PoolDetail::Concentrated(_) => true,
        PoolDetail::Balancer(pool) => {
            if pool.flags & FLAG_STABLE != 0 {
                return true;
            }
            pool.weights.len() == pool.tokens.len()
                && pool.weights.len() >= 2
                && pool.weights.iter().all(|weight| *weight == pool.weights[0])
                && !pool.weights[0].is_zero()
        }
        PoolDetail::Curve(pool) => pool.flags & FLAG_CRYPTO == 0 && pool.gamma.is_zero(),
        PoolDetail::Metric(_) | PoolDetail::Failed(_) => false,
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

pub fn estimate(
    curve: &CurveRecord,
    generation: u64,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    if curve.version != CURVE_VERSION || curve.generation != generation {
        return Err("curve generation".into());
    }
    estimate_payload(
        &curve.payload,
        curve.kind,
        curve.id,
        token_in,
        token_out,
        amount_in,
    )
}

fn estimate_payload(
    payload: &[u8],
    _kind: u8,
    _id: B256,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
) -> Result<Quote, String> {
    let fit = crate::fit_formula::decode_fit(payload)?;
    if fit.reason != 0 {
        return Err("not canonical".into());
    }
    let Some(direction) = fit
        .directions
        .iter()
        .find(|item| item.token_in == token_in && item.token_out == token_out)
    else {
        return Err("no quote".into());
    };
    if amount_in.is_zero() {
        return Ok(Quote {
            amount_in,
            amount_out: U256::ZERO,
            fee_paid: U256::ZERO,
            exhausted: false,
        });
    }
    let input = bigint_from_u256(amount_in);
    let output = crate::fit_select::eval_formula(&direction.formula, &input)
        .ok_or_else(|| "no quote".to_string())?;
    Ok(Quote {
        amount_in,
        amount_out: u256_from_bigint(&output)?,
        fee_paid: u256_from_bigint(&fee_paid_of(&direction.formula, &input))?,
        exhausted: false,
    })
}

/// The fee a product quote withholds from the input.
fn fee_paid_of(formula: &crate::fit_formula::Formula, amount: &BigInt) -> BigInt {
    match formula {
        crate::fit_formula::Formula::Product { fee, denom, .. }
            if !denom.is_zero() && fee <= denom =>
        {
            amount - (amount * (denom - fee)) / denom
        }
        _ => BigInt::from(0),
    }
}

fn bigint_from_u256(value: U256) -> BigInt {
    BigInt::from_bytes_be(Sign::Plus, &value.to_be_bytes::<32>())
}

fn u256_from_bigint(value: &BigInt) -> Result<U256, String> {
    let (sign, bytes) = value.to_bytes_be();
    if sign == Sign::Minus || bytes.len() > 32 {
        return Err("quote".into());
    }
    Ok(U256::from_be_slice(&bytes))
}

/// Classify one log into a dirty hint. Factory creation logs are ignored.
/// The caller resolves identities. A singleton pool id comes from the event.
pub fn classify_log(
    log: &Log,
    mut has_id: impl FnMut(u8, B256) -> bool,
    mut by_address: impl FnMut(Address) -> Option<(u8, B256)>,
) -> Option<WatchHit> {
    let topic = log.topics.first().copied()?;
    let known = crate::decode::known();
    if log.address == known.pool_manager {
        return singleton_hit(
            log,
            topic,
            UNI_V4,
            TOPIC_V4_SWAP,
            TOPIC_V4_LIQUIDITY,
            CLASS_TICKS,
            &mut has_id,
        );
    }
    if log.address == known.bal_v2 {
        return singleton_hit(
            log,
            topic,
            BALANCER_V2,
            TOPIC_BAL_SWAP,
            TOPIC_BAL_BALANCE,
            CLASS_HEAD,
            &mut has_id,
        );
    }
    if log.address == known.bal_v3 && topic == TOPIC_BAL3_SWAP {
        let id = topic_id(log)?;
        if !has_id(BALANCER_V3, id) {
            return None;
        }
        return Some(WatchHit {
            kind: BALANCER_V3,
            id,
            block: log.block_number,
            class: CLASS_HEAD,
        });
    }
    let (kind, id) = by_address(log.address)?;
    let class = if matches!(topic, TOPIC_V3_MINT | TOPIC_V3_BURN | TOPIC_V4_LIQUIDITY) {
        CLASS_TICKS
    } else if matches!(
        topic,
        TOPIC_V2_SYNC
            | TOPIC_V2_SWAP
            | TOPIC_V3_SWAP
            | TOPIC_PANCAKE_SWAP
            | TOPIC_V4_SWAP
            | TOPIC_BAL_SWAP
            | TOPIC_BAL3_SWAP
            | TOPIC_BAL_BALANCE
            | TOPIC_CURVE_EXCHANGE
    ) {
        CLASS_HEAD
    } else if concentrated_kind(kind) || kind == METRIC {
        CLASS_TICKS
    } else {
        CLASS_HEAD
    };
    Some(WatchHit {
        kind,
        id,
        block: log.block_number,
        class,
    })
}

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
            let details = crate::detail::load_details_at(rpc, &page, Some(block))?;
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

/// Choose a curve from local quotes of `detail`. Quoter samples are not read.
pub(crate) fn curve_for_snapshot(
    detail: &PoolDetail,
    generation: u64,
) -> Result<CurveRecord, String> {
    let mut curve = derive_curve(detail, generation)?;
    curve.payload = crate::fit::local_curve_payload(detail)?;
    Ok(curve)
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

/// A quote-stage key. The pipeline no longer enqueues these.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn quote_job(kind: u8, id: B256, generation: u64) -> WorkJob {
    let mut job = blank_job(STAGE_QUOTE, kind, id);
    job.generation = generation;
    job
}

pub(crate) fn keep_or_drop(
    job: &WorkJob,
    error: String,
    give_up_after: u32,
    done: &mut Vec<(u8, u8, B256)>,
    failed: &mut Vec<WorkFail>,
    notes: &mut Vec<String>,
) {
    let error = crate::rpc::redact_secret(&error);
    let kind = crate::catalog::kind_name(job.kind);
    if give_up_after > 0 && job.attempt >= give_up_after {
        observability::error!(
            stage = job.stage,
            kind,
            id = %job.id,
            attempt = job.attempt,
            %error,
            "state gave up"
        );
        notes.push(format!("gave up {kind} {}: {error}", job.id));
        done.push((job.stage, job.kind, job.id));
        return;
    }
    observability::warn!(
        stage = job.stage,
        kind,
        id = %job.id,
        attempt = job.attempt,
        %error,
        "state job failed"
    );
    failed.push(WorkFail {
        stage: job.stage,
        kind: job.kind,
        id: job.id,
        not_before: unix_now().saturating_add(RETRY_SECS),
        error,
    });
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

struct FollowPlan {
    job: Option<WorkJob>,
    derive: bool,
    priority: u64,
}

struct FollowInput<'a> {
    kind: u8,
    id: B256,
    detail: &'a PoolDetail,
    class: u8,
    bitmap_changed: bool,
    truncated: bool,
    tick_count: u32,
    generation: u64,
    watched: bool,
    book_ready: bool,
}

/// What to enqueue after the detail lens returns.
///
/// Outstanding tick pages are still walked. `max_in` for a concentrated book
/// is the ladder, and that ladder needs the tick words the bitmap only names.
fn follow_for_detail(input: &FollowInput<'_>) -> FollowPlan {
    let cap = crate::fit::trade_capacity(input.detail);
    let priority = priority_of(input.watched, cap.top);
    if input.truncated {
        let mut job = blank_job(STAGE_MUTABLE, input.kind, input.id);
        job.priority = priority;
        job.class = input.class;
        return FollowPlan {
            job: Some(job),
            derive: false,
            priority,
        };
    }
    let outstanding = !input.book_ready && concentrated_kind(input.kind) && input.tick_count > 0;
    if outstanding {
        let plan = plan_follow_up(
            input.class,
            input.bitmap_changed,
            false,
            input.tick_count,
            true,
        );
        let Some(stage) = plan.stage.filter(|stage| *stage == STAGE_TICKS) else {
            return FollowPlan {
                job: None,
                derive: cap.any_positive,
                priority,
            };
        };
        let mut job = blank_job(stage, input.kind, input.id);
        job.generation = input.generation;
        job.priority = priority;
        job.class = input.class;
        return FollowPlan {
            job: Some(job),
            derive: false,
            priority,
        };
    }
    if !cap.any_positive {
        return FollowPlan {
            job: None,
            derive: false,
            priority,
        };
    }
    FollowPlan {
        job: None,
        derive: true,
        priority,
    }
}

/// Local curve once the tick words for this generation are stored.
async fn after_book<S: VenueStore>(store: &S, job: &WorkJob) -> eyre::Result<Option<WorkJob>> {
    let state = store.read_state(job.kind, job.id).await?;
    let Some(mutable) = state.mutable else {
        return Ok(Some(blank_job(STAGE_MUTABLE, job.kind, job.id)));
    };
    let Some(immutable) = state.immutable else {
        return Ok(None);
    };
    let detail = match join_detail(&immutable, &mutable.body, &state.ticks, job.kind, job.id) {
        Ok(detail) => detail,
        Err(_) => {
            if curve_is_current(state.curve.as_ref(), mutable.generation) {
                return Ok(None);
            }
            let mut next = curve_job(job.kind, job.id, mutable.generation);
            next.priority = job.priority;
            return Ok(Some(next));
        }
    };
    let cap = crate::fit::trade_capacity(&detail);
    if !cap.any_positive || curve_is_current(state.curve.as_ref(), mutable.generation) {
        return Ok(None);
    }
    let priority = priority_of((job.priority & DIRTY_BAND) != 0, cap.top);
    match curve_for_snapshot(&detail, mutable.generation) {
        Ok(curve) => {
            if store.put_curve(&curve).await.is_err() {
                let mut next = curve_job(job.kind, job.id, mutable.generation);
                next.priority = priority;
                return Ok(Some(next));
            }
            Ok(None)
        }
        Err(_) => {
            let mut next = curve_job(job.kind, job.id, mutable.generation);
            next.priority = priority;
            Ok(Some(next))
        }
    }
}

fn bitmap_fingerprint(bitmaps: &[BitmapWord]) -> B256 {
    let mut bytes = Vec::with_capacity(bitmaps.len() * 34);
    for word in bitmaps {
        bytes.extend_from_slice(&word.word_pos.to_be_bytes());
        bytes.extend_from_slice(&word.bits.to_be_bytes::<32>());
    }
    keccak256(bytes)
}

fn unpack_stored(word: U256) -> (u128, i128) {
    let gross = (word & U256::from(u128::MAX)).to::<u128>();
    let high: U256 = word >> 128;
    let net = high.to::<u128>() as i128;
    (gross, net)
}

fn singleton_hit(
    log: &Log,
    topic: B256,
    kind: u8,
    head: B256,
    other: B256,
    other_class: u8,
    has_id: &mut impl FnMut(u8, B256) -> bool,
) -> Option<WatchHit> {
    if topic != head && topic != other {
        return None;
    }
    let id = topic_id(log)?;
    if !has_id(kind, id) {
        return None;
    }
    Some(WatchHit {
        kind,
        id,
        block: log.block_number,
        class: if topic == other {
            other_class
        } else {
            CLASS_HEAD
        },
    })
}

fn topic_id(log: &Log) -> Option<B256> {
    log.topics.get(1).copied()
}

fn detail_flags(detail: &PoolDetail) -> u64 {
    match detail {
        PoolDetail::ConstantProduct(pool) => pool.flags,
        PoolDetail::Concentrated(pool) => pool.flags,
        PoolDetail::Balancer(pool) => pool.flags,
        PoolDetail::Curve(pool) => pool.flags,
        PoolDetail::Metric(pool) => pool.flags,
        PoolDetail::Failed(pool) => pool.flags,
    }
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

pub(crate) fn skip_reason(detail: &PoolDetail) -> u8 {
    match detail {
        PoolDetail::Metric(_) => 1,
        PoolDetail::Curve(_) => 2,
        PoolDetail::Balancer(_) => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::UNI_V2;
    use crate::Put;
    use alloy_primitives::address;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

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
        PoolDetail::ConstantProduct(crate::detail::ConstantProduct {
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
        assert_eq!(catalog.drop_empty_pools().unwrap().len(), 1);
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
        catalog.put_mutable(&write).unwrap();
    }

    #[test]
    fn deleting_work_rebuilds_the_immutable_job() {
        let (dir, catalog) = open();
        catalog
            .commit_chunk("list", 1, &[row(1)], Put::Merge)
            .unwrap();
        assert_eq!(catalog.reconcile(1).unwrap(), 1);
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
        assert_eq!(catalog.reconcile(2).unwrap(), 1);
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
        catalog.put_mutable(&write).unwrap();
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
        let stored = catalog.put_mutable(&older).unwrap();
        assert!(!stored.dirty_cleared);
        assert_eq!(
            catalog.read_state(UNI_V2, id).unwrap().dirty.unwrap().class,
            CLASS_TICKS
        );
        let mut caught = write;
        caught.block = 14;
        caught.class = CLASS_TICKS;
        let stored = catalog.put_mutable(&caught).unwrap();
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
        let first = catalog.put_mutable(&write).unwrap();
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
        let head = catalog.put_mutable(&write).unwrap();
        assert!(head.kept_ticks);
        assert!(!head.bitmap_changed);
        let state = catalog.read_state(UNI_V3, id).unwrap();
        assert_eq!(state.ticks.len(), 1);
        assert_eq!(state.mutable.unwrap().generation, head.generation);
        write.class = CLASS_TICKS;
        write.block = 5;
        let minted = catalog.put_mutable(&write).unwrap();
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
        let changed = catalog.put_mutable(&write).unwrap();
        assert!(changed.bitmap_changed);
        assert!(catalog.read_state(UNI_V3, id).unwrap().ticks.is_empty());
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
        let stored = catalog.put_mutable(&mutable).unwrap();
        let detail = product();
        let mut curve = derive_curve(&detail, stored.generation).unwrap();
        curve.generation = stored.generation + 1;
        assert!(catalog.put_curve(&curve).is_err());
        curve.generation = stored.generation;
        catalog.put_curve(&curve).unwrap();
        catalog
            .put_quote(&QuoteRecord {
                kind: sample.kind,
                id: sample.id,
                generation: stored.generation,
                version: QUOTE_VERSION,
                block: mutable.block,
                payload: b"{}".to_vec(),
            })
            .unwrap();
        let quote = estimate(
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
        assert_eq!(catalog.reconcile(1).unwrap(), 0);
        mutable.block = 9;
        mutable.class = CLASS_HEAD;
        catalog.put_mutable(&mutable).unwrap();
        let kept = catalog.read_state(sample.kind, sample.id).unwrap();
        assert!(kept.quotes.is_some());
        assert!(kept.curve.is_none());
        assert_eq!(catalog.claim_work(STAGE_QUOTE, 4, 1, 10).unwrap().len(), 0);
        assert!(catalog.reconcile(1).unwrap() >= 1);
        assert_eq!(catalog.claim_work(STAGE_CURVE, 4, 1, 10).unwrap().len(), 1);
        assert!(catalog
            .claim_work(STAGE_QUOTE, 4, 1, 10)
            .unwrap()
            .is_empty());
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
        let stored = catalog.put_mutable(&write).unwrap();
        assert_eq!(catalog.reconcile(1).unwrap(), 1);
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
        assert_eq!(catalog.reconcile(2).unwrap(), 1);
        assert!(catalog
            .claim_work(STAGE_TICKS, 4, 2, 10)
            .unwrap()
            .is_empty());
        assert_eq!(catalog.claim_work(STAGE_CURVE, 4, 2, 10).unwrap().len(), 1);
        assert!(catalog
            .claim_work(STAGE_QUOTE, 4, 2, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_stored_product_curve_uses_the_row_kind() {
        let (_dir, catalog) = open();
        let sample = row(3);
        let mut pancake = product();
        let PoolDetail::ConstantProduct(pool) = &mut pancake else {
            panic!("product");
        };
        pool.kind = crate::catalog::PANCAKE_V2;
        pool.id = sample.id;
        pool.fee_pips = None;
        let (immutable, mut mutable) = split_detail(&pancake).unwrap();
        assert!(!immutable
            .windows(32)
            .any(|word| word == sample.id.as_slice()));
        catalog
            .put_immutable(&[ImmutableRow {
                kind: crate::catalog::PANCAKE_V2,
                id: sample.id,
                body: immutable.clone(),
            }])
            .unwrap();
        mutable.kind = crate::catalog::PANCAKE_V2;
        mutable.id = sample.id;
        mutable.block = 8;
        let stored = catalog.put_mutable(&mutable).unwrap();
        let joined = join_detail(&immutable, &mutable.body, &[], mutable.kind, mutable.id).unwrap();
        let PoolDetail::ConstantProduct(read) = &joined else {
            panic!("joined");
        };
        assert_eq!(read.kind, crate::catalog::PANCAKE_V2);
        assert_eq!(read.id, sample.id);
        let curve = derive_curve(&joined, stored.generation).unwrap();
        assert_eq!(curve.kind, crate::catalog::PANCAKE_V2);
        assert_eq!(curve.id, sample.id);
        catalog.put_curve(&curve).unwrap();
        let amount = U256::from(10_000_000u64);
        let token_in = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let token_out = address!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        let quoted = estimate(&curve, stored.generation, token_in, token_out, amount).unwrap();
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
        let head = plan_follow_up(CLASS_HEAD, false, false, 10, true);
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
            address: crate::decode::known().uni_v3,
            topics: vec![TOPIC_V3_SWAP],
            ..mint.clone()
        };
        assert!(classify_log(&factory, has, address).is_none());
    }

    #[test]
    fn an_uninitialized_pool_stores_a_skipped_curve() {
        let detail = PoolDetail::Concentrated(crate::detail::Concentrated {
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
        key[0] = STAGE_QUOTE;
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
        assert_eq!(job.stage, STAGE_QUOTE);
    }

    #[test]
    fn an_empty_snapshot_drops_follow_up_and_keeps_the_identity() {
        let (_dir, catalog) = open();
        let (venue, detail) = with_reserves(3, 0);
        store_detail(&catalog, &venue, &detail);
        let mut seeded = quote_job(venue.kind, venue.id, 1);
        seeded.priority = 50;
        catalog
            .enqueue_work(std::slice::from_ref(&seeded), 1)
            .unwrap();
        catalog.reconcile(1).unwrap();
        assert!(catalog
            .claim_work(STAGE_QUOTE, 4, 1, 10)
            .unwrap()
            .is_empty());
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
    fn a_one_wei_pool_derives_locally_and_skips_the_quoter() {
        let (_dir, catalog) = open();
        let (venue, detail) = with_reserves(2, 1);
        store_detail(&catalog, &venue, &detail);
        catalog.reconcile(1).unwrap();
        assert!(catalog
            .claim_work(STAGE_QUOTE, 4, 1, 10)
            .unwrap()
            .is_empty());
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
        catalog.reconcile(1).unwrap();
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
        let mut quiet = quote_job(UNI_V2, row(1).id, 1);
        quiet.priority = priority_of(false, U256::from(u64::MAX));
        let mut dirty = quote_job(UNI_V2, row(9).id, 1);
        dirty.priority = priority_of(true, U256::from(1u64));
        catalog.enqueue_work(&[quiet, dirty], 1).unwrap();
        let claimed = catalog.claim_work(STAGE_QUOTE, 1, 1, 10).unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].id, row(9).id);
        assert!(claimed[0].priority >= DIRTY_BAND);
    }

    #[test]
    fn a_leased_job_is_not_replaced() {
        let (_dir, catalog) = open();
        let mut job = quote_job(UNI_V2, row(1).id, 1);
        job.priority = 1;
        catalog.enqueue_work(std::slice::from_ref(&job), 1).unwrap();
        assert_eq!(catalog.claim_work(STAGE_QUOTE, 1, 1, 100).unwrap().len(), 1);
        let mut hotter = job;
        hotter.priority = 99;
        hotter.lease_until = 0;
        catalog
            .enqueue_work(std::slice::from_ref(&hotter), 1)
            .unwrap();
        assert!(catalog
            .claim_work(STAGE_QUOTE, 1, 1, 10)
            .unwrap()
            .is_empty());
        let later = catalog.claim_work(STAGE_QUOTE, 1, 101, 10).unwrap();
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
        let detail = PoolDetail::Concentrated(crate::detail::Concentrated {
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
        catalog.reconcile(1).unwrap();
        assert_eq!(catalog.claim_work(STAGE_TICKS, 4, 1, 10).unwrap().len(), 1);
        assert!(catalog
            .claim_work(STAGE_QUOTE, 4, 1, 10)
            .unwrap()
            .is_empty());
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
