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

use crate::catalog::{entity_key, AERO, BALANCER_V2, BALANCER_V3, CURVE, METRIC, UNI_V4, VENUES};
use crate::detail::{
    tick_request, BitmapWord, PoolDetail, Quote, TickWord, FLAG_CRYPTO, FLAG_FAILED, FLAG_STABLE,
    FLAG_TRUNCATED,
};
use crate::rpc::Rpc;
use crate::tick::{self, TickWordRead};
use crate::{
    Catalog, EntityStore, RunOpts, RunReport, VenueRow, PANCAKE_V3, SUSHI_V3, UNI_V3, V3_FORK,
};
use alloy_primitives::{keccak256, Address, B256, U256};
use eyre::WrapErr;
use ipc::HistoryPeer;
use model::{BlockRecord, NormalizedLog};
use num_bigint::{BigInt, Sign};
use num_traits::Zero;
use redb::{Durability, ReadableDatabase, ReadableTable, TableDefinition};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod codec;
mod drive;

pub(crate) use self::codec::decode_curve_record;
use self::codec::*;
pub use self::drive::run;

const IMMUTABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("immutable");
const MUTABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mutable");
const TICK_PAGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tick_pages");
const CURVES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("curves");
const QUOTES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("quotes");
const DIRTY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("dirty");
const MISMATCH: TableDefinition<&[u8], &[u8]> = TableDefinition::new("immutable_mismatch");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const JOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("jobs");
/// Claim order for one stage. The identity key stays on `JOBS`.
const JOB_RANK: TableDefinition<&[u8], &[u8]> = TableDefinition::new("job_rank");
/// Watch hits sort ahead of every quiet pool. The low 63 bits are `max_in`.
const DIRTY_BAND: u64 = 1 << 63;

const WATCH_KEY: &str = "watch_through";
/// Fit-document generation stored on a curve. A bump rebuilds every curve.
/// Payload schema. A version 3 curve is chosen from local quotes of the
/// ingested snapshot. Older rows were fit to quoter samples or left empty.
pub const CURVE_VERSION: u32 = 3;
/// Sampling scheme stored on a quote row. A bump re-reads every quoter.
pub const QUOTE_VERSION: u32 = 1;
pub const TICK_PAGE: u32 = 512;
pub const STAGE_IMMUTABLE: u8 = 1;
pub const STAGE_MUTABLE: u8 = 2;
pub const STAGE_TICKS: u8 = 3;
pub const STAGE_CURVE: u8 = 4;
/// Retained so older work rows can be dropped. The pipeline does not claim this stage.
pub const STAGE_QUOTE: u8 = 5;
/// Pair conduit. Claimed after the curve batch, from `conduit_jobs`.
pub const STAGE_CONDUIT: u8 = 6;
pub const CLASS_NONE: u8 = 0;
pub const CLASS_HEAD: u8 = 1;
pub const CLASS_TICKS: u8 = 2;
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
pub enum ImmutablePut {
    Written,
    Present,
    Mismatch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImmutableRow {
    pub kind: u8,
    pub id: B256,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableWrite {
    pub kind: u8,
    pub id: B256,
    pub block: u64,
    pub class: u8,
    pub bitmap: B256,
    pub tick_count: u32,
    pub truncated: bool,
    pub next_scan: Option<i32>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableStored {
    pub generation: u64,
    pub dirty_cleared: bool,
    pub kept_ticks: bool,
    pub bitmap_changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableView {
    pub block: u64,
    pub generation: u64,
    pub bitmap: B256,
    pub tick_count: u32,
    pub truncated: bool,
    pub next_scan: Option<i32>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickPage {
    pub kind: u8,
    pub id: B256,
    pub page: u32,
    pub generation: u64,
    pub ticks: Vec<TickWord>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveRecord {
    pub kind: u8,
    pub id: B256,
    pub generation: u64,
    pub version: u32,
    pub payload: Vec<u8>,
}

/// Chain quote samples for one snapshot. `payload` is the direction grid.
/// Local quote outputs are not stored; they are recomputed from the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteRecord {
    pub kind: u8,
    pub id: B256,
    pub generation: u64,
    pub version: u32,
    pub block: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirty {
    pub block: u64,
    pub class: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchHit {
    pub kind: u8,
    pub id: B256,
    pub block: u64,
    pub class: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkJob {
    pub stage: u8,
    pub kind: u8,
    pub id: B256,
    pub lease_until: u64,
    pub attempt: u32,
    pub not_before: u64,
    pub generation: u64,
    pub tick_cursor: i32,
    pub class: u8,
    /// Higher is claimed first. Bit 63 is a watch hit. The low bits grow with `max_in`.
    pub priority: u64,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkFail {
    pub stage: u8,
    pub kind: u8,
    pub id: B256,
    pub not_before: u64,
    pub error: String,
}

/// Counts for a sample catalog after a state pass.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StateSummary {
    pub identities: u64,
    pub immutable: u64,
    pub mutable: u64,
    pub curves: u64,
    pub conduits: u64,
    pub quotes: u64,
    pub tick_pages: u64,
    pub mismatches: u64,
    pub jobs: u64,
    pub jobs_by_stage: BTreeMap<u8, u64>,
    pub errors: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub venue: Option<VenueRow>,
    pub immutable: Option<Vec<u8>>,
    pub mutable: Option<MutableView>,
    pub ticks: Vec<TickWord>,
    pub curve: Option<CurveRecord>,
    pub quotes: Option<QuoteRecord>,
    pub dirty: Option<Dirty>,
}

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
    log: &NormalizedLog,
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

impl Catalog {
    pub fn state_summary(&self) -> eyre::Result<StateSummary> {
        let mut summary = StateSummary {
            identities: count_bytes(&self.entities, VENUES)?,
            immutable: count_bytes(&self.entities, IMMUTABLE)?,
            mutable: count_bytes(&self.entities, MUTABLE)?,
            curves: count_bytes(&self.entities, CURVES)?,
            conduits: crate::conduit::count_conduits(&self.entities)?,
            quotes: count_bytes(&self.entities, QUOTES)?,
            tick_pages: count_bytes(&self.entities, TICK_PAGES)?,
            mismatches: count_bytes(&self.entities, MISMATCH)?,
            ..StateSummary::default()
        };
        let read = self.work.begin_read()?;
        let table = match read.open_table(JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(summary),
            Err(err) => return Err(err.into()),
        };
        for item in table.iter()? {
            let (key, value) = item?;
            let job = decode_job(key.value(), value.value())?;
            summary.jobs += 1;
            *summary.jobs_by_stage.entry(job.stage).or_default() += 1;
            if !job.error.is_empty() {
                *summary.errors.entry(job.error).or_default() += 1;
            }
        }
        let (conduit_jobs, conduit_errors) = crate::conduit::account_jobs(&self.work)?;
        summary.jobs += conduit_jobs;
        if conduit_jobs > 0 {
            summary.jobs_by_stage.insert(STAGE_CONDUIT, conduit_jobs);
        }
        for (error, count) in conduit_errors {
            *summary.errors.entry(error).or_default() += count;
        }
        Ok(summary)
    }

    pub fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        crate::catalog::read_venue_row(&self.entities, kind, id)
    }

    pub fn watch_through(&self) -> eyre::Result<Option<u64>> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(WATCH_KEY)?.map(|value| value.value()))
    }

    /// Record dirty flags and advance the watch cursor in one transaction.
    ///
    /// `through` is the first block not included. A hit at or after `through`
    /// is rejected, so the cursor cannot pass an event this call did not record.
    /// A pool with no snapshot, or a snapshot already at this block, is not
    /// dirtied. The cursor still advances.
    pub fn apply_watch(&self, through: u64, hits: &[WatchHit]) -> eyre::Result<()> {
        if let Some(current) = self.watch_through()? {
            if through < current {
                return Err(eyre::eyre!(
                    "watch cursor is {current}; {through} is below it"
                ));
            }
        }
        if hits.iter().any(|hit| hit.block >= through) {
            return Err(eyre::eyre!(
                "watch hit is outside the committed range ..{through}"
            ));
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let bumped = {
            let mut dirty_table = write.open_table(DIRTY)?;
            let mutable_table = write.open_table(MUTABLE)?;
            let mut meta = write.open_table(META)?;
            let mut merged: BTreeMap<(u8, B256), (u64, u8)> = BTreeMap::new();
            for hit in hits {
                if !event_after_snapshot(&mutable_table, hit)? {
                    continue;
                }
                let entry = merged.entry((hit.kind, hit.id)).or_insert((0, CLASS_HEAD));
                entry.0 = entry.0.max(hit.block);
                if hit.class == CLASS_TICKS {
                    entry.1 = CLASS_TICKS;
                }
            }
            let mut bumped = Vec::with_capacity(merged.len());
            for ((kind, id), (block, class)) in merged {
                let key = entity_key(kind, &id);
                let (seen, stored_class) = match dirty_table.get(key.as_slice())? {
                    Some(value) => decode_dirty(value.value())?,
                    None => (0, CLASS_NONE),
                };
                let class = if stored_class == CLASS_TICKS || class == CLASS_TICKS {
                    CLASS_TICKS
                } else {
                    CLASS_HEAD
                };
                let bytes = encode_dirty(seen.max(block), class);
                dirty_table.insert(key.as_slice(), bytes.as_slice())?;
                bumped.push((kind, id, class));
            }
            meta.insert(WATCH_KEY, through)?;
            bumped
        };
        write.commit()?;
        self.enqueue_bumped(&bumped)?;
        Ok(())
    }

    pub fn put_immutable(&self, rows: &[ImmutableRow]) -> eyre::Result<Vec<ImmutablePut>> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let mut puts = Vec::with_capacity(rows.len());
        {
            let mut table = write.open_table(IMMUTABLE)?;
            let mut mismatch = write.open_table(MISMATCH)?;
            for row in rows {
                let key = entity_key(row.kind, &row.id);
                let stored = table
                    .get(key.as_slice())?
                    .map(|value| value.value().to_vec());
                match stored.as_deref() {
                    None => {
                        table.insert(key.as_slice(), row.body.as_slice())?;
                        puts.push(ImmutablePut::Written);
                    }
                    Some(bytes) if bytes == row.body.as_slice() => {
                        puts.push(ImmutablePut::Present);
                    }
                    Some(_) => {
                        let count = match mismatch.get(key.as_slice())? {
                            Some(value) => u64::from_le_bytes(word8(value.value())?),
                            None => 0,
                        }
                        .saturating_add(1);
                        mismatch.insert(key.as_slice(), count.to_le_bytes().as_slice())?;
                        puts.push(ImmutablePut::Mismatch);
                    }
                }
            }
        }
        write.commit()?;
        Ok(puts)
    }

    pub fn immutable_mismatch(&self, kind: u8, id: B256) -> eyre::Result<u64> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(MISMATCH) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        let key = entity_key(kind, &id);
        match table.get(key.as_slice())? {
            Some(value) => Ok(u64::from_le_bytes(word8(value.value())?)),
            None => Ok(0),
        }
    }

    /// Write one mutable snapshot. Tick pages from an older bitmap, or from a
    /// mint, are deleted here. A head refresh with the same bitmap keeps the
    /// pages and stamps the new generation on them. The curve is always dropped.
    /// Stored chain samples stay. They are validation data for the snapshot
    /// that produced them, and a later generation does not treat them as current.
    /// The dirty flag is removed only when it does not name a newer block.
    pub fn put_mutable(&self, row: &MutableWrite) -> eyre::Result<MutableStored> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let (stored, previous_curve) = {
            let mut mutable_table = write.open_table(MUTABLE)?;
            let mut ticks = write.open_table(TICK_PAGES)?;
            let mut curves = write.open_table(CURVES)?;
            let mut dirty_table = write.open_table(DIRTY)?;
            let key = entity_key(row.kind, &row.id);
            let previous = match mutable_table.get(key.as_slice())? {
                Some(value) => Some(decode_mutable(value.value())?),
                None => None,
            };
            let previous_curve = match curves.get(key.as_slice())? {
                Some(value) => decode_curve_record(row.kind, row.id, value.value()).ok(),
                None => None,
            };
            let generation = previous
                .as_ref()
                .map(|view| view.generation.saturating_add(1))
                .unwrap_or(1);
            let bitmap_changed = previous
                .as_ref()
                .map(|view| view.bitmap != row.bitmap)
                .unwrap_or(true);
            let kept_ticks = !bitmap_changed && row.class == CLASS_HEAD && !row.truncated;
            if kept_ticks {
                restamp_ticks(&mut ticks, row.kind, row.id, generation)?;
            } else {
                delete_prefix(&mut ticks, &key)?;
            }
            let _ = curves.remove(key.as_slice())?;
            let bytes = encode_mutable(row, generation);
            mutable_table.insert(key.as_slice(), bytes.as_slice())?;
            let seen = match dirty_table.get(key.as_slice())? {
                Some(value) => Some(decode_dirty(value.value())?.0),
                None => None,
            };
            let dirty_cleared = match seen {
                Some(seen) if seen <= row.block => {
                    dirty_table.remove(key.as_slice())?;
                    true
                }
                _ => false,
            };
            (
                MutableStored {
                    generation,
                    dirty_cleared,
                    kept_ticks,
                    bitmap_changed,
                },
                previous_curve,
            )
        };
        write.commit()?;
        if let Some(previous_curve) = previous_curve {
            crate::conduit::retarget(self, row.kind, row.id, Some(&previous_curve), None)?;
        }
        Ok(stored)
    }

    pub fn put_tick_pages(&self, pages: &[TickPage]) -> eyre::Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut ticks = write.open_table(TICK_PAGES)?;
            for page in pages {
                let key = entity_key(page.kind, &page.id);
                let Some(stored) = mutable_table.get(key.as_slice())? else {
                    return Err(eyre::eyre!("tick page has no mutable row"));
                };
                let view = decode_mutable(stored.value())?;
                if view.generation != page.generation {
                    return Err(eyre::eyre!("tick page generation"));
                }
                let bytes = encode_tick_page(page)?;
                ticks.insert(
                    tick_key(page.kind, page.id, page.page).as_slice(),
                    bytes.as_slice(),
                )?;
            }
        }
        write.commit()?;
        Ok(())
    }

    pub fn put_curve(&self, curve: &CurveRecord) -> eyre::Result<()> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let previous = {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut curves = write.open_table(CURVES)?;
            let key = entity_key(curve.kind, &curve.id);
            let Some(stored) = mutable_table.get(key.as_slice())? else {
                return Err(eyre::eyre!("curve has no mutable row"));
            };
            let view = decode_mutable(stored.value())?;
            if view.generation != curve.generation {
                return Err(eyre::eyre!("curve generation"));
            }
            let previous = match curves.get(key.as_slice())? {
                Some(value) => decode_curve_record(curve.kind, curve.id, value.value()).ok(),
                None => None,
            };
            let bytes = encode_curve(curve);
            curves.insert(key.as_slice(), bytes.as_slice())?;
            previous
        };
        write.commit()?;
        crate::conduit::retarget(self, curve.kind, curve.id, previous.as_ref(), Some(curve))?;
        Ok(())
    }

    /// Store a page of curves in one commit. A generation mismatch stores none of them.
    pub fn put_curves(&self, curves: &[CurveRecord]) -> eyre::Result<()> {
        if curves.is_empty() {
            return Ok(());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let mut replaced = Vec::with_capacity(curves.len());
        {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut table = write.open_table(CURVES)?;
            for curve in curves {
                let key = entity_key(curve.kind, &curve.id);
                let Some(stored) = mutable_table.get(key.as_slice())? else {
                    return Err(eyre::eyre!("curve has no mutable row"));
                };
                let view = decode_mutable(stored.value())?;
                if view.generation != curve.generation {
                    return Err(eyre::eyre!("curve generation"));
                }
                let previous = match table.get(key.as_slice())? {
                    Some(value) => decode_curve_record(curve.kind, curve.id, value.value()).ok(),
                    None => None,
                };
                let bytes = encode_curve(curve);
                table.insert(key.as_slice(), bytes.as_slice())?;
                replaced.push(previous);
            }
        }
        write.commit()?;
        for (previous, curve) in replaced.iter().zip(curves) {
            crate::conduit::retarget(self, curve.kind, curve.id, previous.as_ref(), Some(curve))?;
        }
        Ok(())
    }

    pub fn put_quote(&self, quote: &QuoteRecord) -> eyre::Result<()> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mutable_table = write.open_table(MUTABLE)?;
            let mut quotes = write.open_table(QUOTES)?;
            let key = entity_key(quote.kind, &quote.id);
            let Some(stored) = mutable_table.get(key.as_slice())? else {
                return Err(eyre::eyre!("quote has no mutable row"));
            };
            let view = decode_mutable(stored.value())?;
            if view.generation != quote.generation || view.block != quote.block {
                return Err(eyre::eyre!("quote generation"));
            }
            let bytes = encode_quote(quote);
            quotes.insert(key.as_slice(), bytes.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }

    /// Store a page of quotes in one commit. A generation mismatch stores none of them.
    pub fn put_quotes(&self, quotes: &[QuoteRecord]) -> eyre::Result<()> {
        if quotes.is_empty() {
            return Ok(());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mutable_table = write.open_table(MUTABLE)?;
            for quote in quotes {
                let key = entity_key(quote.kind, &quote.id);
                let Some(stored) = mutable_table.get(key.as_slice())? else {
                    return Err(eyre::eyre!("quote has no mutable row"));
                };
                let view = decode_mutable(stored.value())?;
                if view.generation != quote.generation || view.block != quote.block {
                    return Err(eyre::eyre!("quote generation"));
                }
            }
            let mut quote_table = write.open_table(QUOTES)?;
            for quote in quotes {
                let key = entity_key(quote.kind, &quote.id);
                let bytes = encode_quote(quote);
                quote_table.insert(key.as_slice(), bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// Delete identities whose stored snapshot cannot price any positive input.
    ///
    /// Pools with no snapshot yet stay, so a fill can be ingested afterwards.
    /// A one-wei reserve stays. Zero reserves and an empty book do not.
    pub fn drop_empty_pools(&self) -> eyre::Result<Vec<(u8, B256)>> {
        let mut drop = Vec::new();
        for kind in 1..=METRIC {
            let mut after = None;
            loop {
                let page = self.page_venues(kind, after, 64)?;
                if page.is_empty() {
                    break;
                }
                after = Some(page.last().expect("page").id);
                for row in &page {
                    let state = self.read_state(row.kind, row.id)?;
                    if !crate::fit::has_trade_size(row.kind, row.id, state) {
                        drop.push((row.kind, row.id));
                    }
                }
                if page.len() < 64 {
                    break;
                }
            }
        }
        self.delete_pools(&drop)?;
        Ok(drop)
    }

    /// Delete a stride of `listed` identities until `target` rows remain.
    ///
    /// Identities that are not in `listed` stay. The returned list is `listed`
    /// without the rows that were removed.
    pub fn trim_listed(
        &self,
        target: u64,
        listed: &[(u8, B256)],
    ) -> eyre::Result<(u64, Vec<(u8, B256)>)> {
        let have = self.state_summary()?.identities;
        if have <= target {
            return Ok((0, listed.to_vec()));
        }
        let excess = (have - target) as usize;
        let mut present = Vec::new();
        for (kind, id) in listed {
            if self.read_venue(*kind, *id)?.is_some() {
                present.push((*kind, *id));
            }
        }
        let remove = stride_pick(&present, excess.min(present.len()));
        let removed = self.delete_pools(&remove)?;
        let gone: std::collections::HashSet<(u8, B256)> = remove.into_iter().collect();
        let left = listed
            .iter()
            .copied()
            .filter(|id| !gone.contains(id))
            .collect();
        Ok((removed, left))
    }

    /// Remove identities and every state row that hangs off them.
    pub fn delete_pools(&self, ids: &[(u8, B256)]) -> eyre::Result<u64> {
        let mut removed = 0_u64;
        for chunk in ids.chunks(128) {
            let mut write = self.entities.begin_write()?;
            write.set_durability(Durability::Immediate)?;
            let mut dropped_curves = Vec::new();
            {
                let mut venues = write.open_table(VENUES)?;
                let mut immutable = write.open_table(IMMUTABLE)?;
                let mut mutable = write.open_table(MUTABLE)?;
                let mut ticks = write.open_table(TICK_PAGES)?;
                let mut curves = write.open_table(CURVES)?;
                let mut quotes = write.open_table(QUOTES)?;
                let mut dirty = write.open_table(DIRTY)?;
                let mut mismatch = write.open_table(MISMATCH)?;
                for (kind, id) in chunk {
                    let key = entity_key(*kind, id);
                    if venues.remove(key.as_slice())?.is_some() {
                        removed += 1;
                    }
                    if let Some(value) = curves.get(key.as_slice())? {
                        if let Ok(curve) = decode_curve_record(*kind, *id, value.value()) {
                            dropped_curves.push(curve);
                        }
                    }
                    immutable.remove(key.as_slice())?;
                    mutable.remove(key.as_slice())?;
                    curves.remove(key.as_slice())?;
                    quotes.remove(key.as_slice())?;
                    dirty.remove(key.as_slice())?;
                    mismatch.remove(key.as_slice())?;
                    delete_prefix(&mut ticks, &key)?;
                }
            }
            write.commit()?;
            for curve in &dropped_curves {
                crate::conduit::retarget(self, curve.kind, curve.id, Some(curve), None)?;
            }
            let mut work = self.work.begin_write()?;
            work.set_durability(Durability::Immediate)?;
            {
                let mut jobs = work.open_table(JOBS)?;
                let mut ranks = work.open_table(JOB_RANK)?;
                for (kind, id) in chunk {
                    for stage in STAGE_IMMUTABLE..=STAGE_QUOTE {
                        clear_job(&mut jobs, &mut ranks, stage, *kind, id)?;
                    }
                }
            }
            work.commit()?;
        }
        Ok(removed)
    }

    /// Remove one quote row. The snapshot and the curve stay.
    pub fn delete_quote(&self, kind: u8, id: B256) -> eyre::Result<bool> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let removed = {
            let mut quotes = write.open_table(QUOTES)?;
            let key = entity_key(kind, &id);
            let removed = quotes.remove(key.as_slice())?.is_some();
            removed
        };
        write.commit()?;
        Ok(removed)
    }

    pub fn read_state(&self, kind: u8, id: B256) -> eyre::Result<PoolState> {
        let venue = self.read_venue(kind, id)?;
        let read = self.entities.begin_read()?;
        let key = entity_key(kind, &id);
        let immutable = read_optional(&read, IMMUTABLE, &key)?;
        let mutable = match read_optional(&read, MUTABLE, &key)? {
            Some(bytes) => Some(decode_mutable(&bytes)?),
            None => None,
        };
        let curve = match read_optional(&read, CURVES, &key)? {
            Some(bytes) => Some(decode_curve_record(kind, id, &bytes)?),
            None => None,
        };
        let quotes = match read_optional(&read, QUOTES, &key)? {
            Some(bytes) => Some(decode_quote_record(kind, id, &bytes)?),
            None => None,
        };
        let dirty = match read_optional(&read, DIRTY, &key)? {
            Some(bytes) => {
                let (block, class) = decode_dirty(&bytes)?;
                Some(Dirty { block, class })
            }
            None => None,
        };
        let ticks = read_tick_words(&read, kind, id)?;
        Ok(PoolState {
            venue,
            immutable,
            mutable,
            ticks,
            curve,
            quotes,
            dirty,
        })
    }

    /// Insert jobs for identities whose stored state is behind.
    ///
    /// An existing lease is left alone. This is the resume path after
    /// `work.redb` is deleted, not the steady-state loop.
    pub fn reconcile(&self, now: u64) -> eyre::Result<u64> {
        let mut inserted = 0_u64;
        for kind in 1..=METRIC {
            let mut after = None;
            loop {
                let page = self.page_venues(kind, after, 256)?;
                if page.is_empty() {
                    break;
                }
                after = Some(page.last().expect("page").id);
                inserted += self.reconcile_rows(&page, now)?;
                if page.len() < 256 {
                    break;
                }
            }
        }
        self.index_ranks()?;
        Ok(inserted)
    }

    pub fn claim_work(
        &self,
        stage: u8,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<WorkJob>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let jobs = {
            let mut table = write.open_table(JOBS)?;
            let mut ranks = write.open_table(JOB_RANK)?;
            let mut picked = Vec::new();
            let mut stale = Vec::new();
            let start = rank_key(stage, u64::MAX, 0, &B256::ZERO);
            let end = rank_key(stage.saturating_add(1), u64::MAX, 0, &B256::ZERO);
            let mut saw_rank = false;
            for item in ranks.range::<&[u8]>(start.as_slice()..end.as_slice())? {
                saw_rank = true;
                if picked.len() == limit as usize {
                    break;
                }
                let (key, _) = item?;
                let Some((rank_stage, priority, kind, id)) = decode_rank(key.value()) else {
                    continue;
                };
                let identity = job_key(rank_stage, kind, &id);
                let Some(stored) = table.get(identity.as_slice())? else {
                    stale.push(key.value().to_vec());
                    continue;
                };
                let mut job = decode_job(identity.as_slice(), stored.value())?;
                drop(stored);
                if job.priority != priority {
                    stale.push(key.value().to_vec());
                    continue;
                }
                if !lease_job(&mut job, now, lease_for) {
                    continue;
                }
                picked.push((identity.to_vec(), job));
            }
            if !saw_rank {
                let start = job_key(stage, 0, &B256::ZERO);
                let end = job_key(stage.saturating_add(1), 0, &B256::ZERO);
                for item in table.range::<&[u8]>(start.as_slice()..end.as_slice())? {
                    if picked.len() == limit as usize {
                        break;
                    }
                    let (key, value) = item?;
                    let mut job = decode_job(key.value(), value.value())?;
                    if !lease_job(&mut job, now, lease_for) {
                        continue;
                    }
                    picked.push((key.value().to_vec(), job));
                }
            }
            for key in stale {
                ranks.remove(key.as_slice())?;
            }
            let mut jobs = Vec::with_capacity(picked.len());
            for (key, job) in picked {
                let bytes = encode_job(&job);
                table.insert(key.as_slice(), bytes.as_slice())?;
                jobs.push(job);
            }
            jobs
        };
        write.commit()?;
        Ok(jobs)
    }

    pub fn enqueue_work(&self, jobs: &[WorkJob], now: u64) -> eyre::Result<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        self.sync_jobs(&[], jobs, now)?;
        Ok(())
    }

    pub fn finish_work(&self, done: &[(u8, u8, B256)], failed: &[WorkFail]) -> eyre::Result<()> {
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut table = write.open_table(JOBS)?;
            let mut ranks = write.open_table(JOB_RANK)?;
            for (stage, kind, id) in done {
                clear_job(&mut table, &mut ranks, *stage, *kind, id)?;
            }
            for fail in failed {
                let key = job_key(fail.stage, fail.kind, &fail.id);
                let Some(stored) = table.get(key.as_slice())? else {
                    continue;
                };
                let mut job = decode_job(key.as_slice(), stored.value())?;
                drop(stored);
                job.lease_until = 0;
                job.not_before = fail.not_before;
                job.error = clip_error(&fail.error);
                put_job(&mut table, &mut ranks, &job)?;
            }
        }
        write.commit()?;
        Ok(())
    }

    fn reconcile_rows(&self, rows: &[VenueRow], now: u64) -> eyre::Result<u64> {
        let mut jobs = Vec::new();
        let mut drop = Vec::new();
        for row in rows {
            let state = self.read_state(row.kind, row.id)?;
            let desired = self.desired_job(row, &state)?;
            for stage in STAGE_IMMUTABLE..=STAGE_QUOTE {
                if desired.as_ref().is_some_and(|job| job.stage == stage) {
                    continue;
                }
                drop.push((stage, row.kind, row.id));
            }
            if let Some(job) = desired {
                jobs.push(job);
            }
        }
        self.sync_jobs(&drop, &jobs, now)
    }

    /// One mutable job per pool the watch just marked, at the dirty priority.
    ///
    /// The entity commit has already landed. A leased job is left alone.
    fn enqueue_bumped(&self, bumped: &[(u8, B256, u8)]) -> eyre::Result<()> {
        if bumped.is_empty() {
            return Ok(());
        }
        let mut jobs = Vec::with_capacity(bumped.len());
        for (kind, id, class) in bumped {
            let state = self.read_state(*kind, *id)?;
            let top = stored_capacity(*kind, *id, &state)
                .map(|cap| cap.top)
                .unwrap_or(U256::ZERO);
            let mut job = blank_job(STAGE_MUTABLE, *kind, *id);
            job.class = *class;
            job.priority = priority_of(true, top);
            if let Some(mutable) = &state.mutable {
                job.generation = mutable.generation;
            }
            jobs.push(job);
        }
        self.enqueue_work(&jobs, unix_now())
    }

    fn desired_job(&self, row: &VenueRow, state: &PoolState) -> eyre::Result<Option<WorkJob>> {
        if state.immutable.is_none() {
            return Ok(Some(blank_job(STAGE_IMMUTABLE, row.kind, row.id)));
        }
        let Some(mutable) = &state.mutable else {
            return Ok(Some(blank_job(STAGE_MUTABLE, row.kind, row.id)));
        };
        if state.dirty.is_some() || mutable.truncated {
            let top = stored_capacity(row.kind, row.id, state)
                .map(|cap| cap.top)
                .unwrap_or(U256::ZERO);
            let mut job = blank_job(STAGE_MUTABLE, row.kind, row.id);
            job.generation = mutable.generation;
            job.class = state
                .dirty
                .as_ref()
                .map(|dirty| dirty.class)
                .unwrap_or(CLASS_NONE);
            job.priority = priority_of(state.dirty.is_some(), top);
            return Ok(Some(job));
        }
        let top = match stored_capacity(row.kind, row.id, state) {
            Some(cap) => {
                if needs_ticks(row.kind, mutable) {
                    let cursor = first_missing_page(&self.entities, row.kind, row.id, mutable)?;
                    let pages = mutable.tick_count.div_ceil(TICK_PAGE);
                    if cursor < pages {
                        let mut job = blank_job(STAGE_TICKS, row.kind, row.id);
                        job.generation = mutable.generation;
                        job.tick_cursor = i32::try_from(cursor).unwrap_or(i32::MAX);
                        job.priority = priority_of(false, cap.top);
                        return Ok(Some(job));
                    }
                }
                if !cap.any_positive {
                    return Ok(None);
                }
                if !cap.fills_sample {
                    if curve_is_current(state.curve.as_ref(), mutable.generation) {
                        return Ok(None);
                    }
                    let mut job = curve_job(row.kind, row.id, mutable.generation);
                    job.priority = priority_of(false, cap.top);
                    return Ok(Some(job));
                }
                cap.top
            }
            None => return self.structural_job(row, mutable, state, 0),
        };
        self.structural_job(row, mutable, state, priority_of(false, top))
    }

    /// Tick, then a local curve. Chain samples are not requested.
    fn structural_job(
        &self,
        row: &VenueRow,
        mutable: &MutableView,
        state: &PoolState,
        priority: u64,
    ) -> eyre::Result<Option<WorkJob>> {
        if needs_ticks(row.kind, mutable) {
            let cursor = first_missing_page(&self.entities, row.kind, row.id, mutable)?;
            let pages = mutable.tick_count.div_ceil(TICK_PAGE);
            if cursor < pages {
                let mut job = blank_job(STAGE_TICKS, row.kind, row.id);
                job.generation = mutable.generation;
                job.tick_cursor = i32::try_from(cursor).unwrap_or(i32::MAX);
                job.priority = priority;
                return Ok(Some(job));
            }
        }
        if !curve_is_current(state.curve.as_ref(), mutable.generation) {
            let mut job = curve_job(row.kind, row.id, mutable.generation);
            job.priority = priority;
            return Ok(Some(job));
        }
        Ok(None)
    }

    fn sync_jobs(&self, drop: &[(u8, u8, B256)], jobs: &[WorkJob], now: u64) -> eyre::Result<u64> {
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let written = {
            let mut table = write.open_table(JOBS)?;
            let mut ranks = write.open_table(JOB_RANK)?;
            for (stage, kind, id) in drop {
                let Some(existing) = load_job(&table, *stage, *kind, id)? else {
                    continue;
                };
                if existing.lease_until > now {
                    ensure_rank(&mut ranks, &existing)?;
                    continue;
                }
                clear_job(&mut table, &mut ranks, *stage, *kind, id)?;
            }
            let mut written = 0_u64;
            for job in jobs {
                if let Some(existing) = load_job(&table, job.stage, job.kind, &job.id)? {
                    if existing.lease_until > now {
                        ensure_rank(&mut ranks, &existing)?;
                        continue;
                    }
                    if existing.priority != job.priority {
                        remove_rank(&mut ranks, &existing)?;
                    }
                }
                put_job(&mut table, &mut ranks, job)?;
                written += 1;
            }
            written
        };
        write.commit()?;
        Ok(written)
    }

    /// Give every stored job a rank key. Startup finds leased rows the venue
    /// walk did not rewrite.
    fn index_ranks(&self) -> eyre::Result<()> {
        let read = self.work.begin_read()?;
        let jobs = match read.open_table(JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let ranks = match read.open_table(JOB_RANK) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(err) => return Err(err.into()),
        };
        let mut missing = Vec::new();
        for item in jobs.iter()? {
            let (key, value) = item?;
            let job = decode_job(key.value(), value.value())?;
            let rank = rank_key(job.stage, job.priority, job.kind, &job.id);
            let present = match &ranks {
                Some(table) => table.get(rank.as_slice())?.is_some(),
                None => false,
            };
            if !present {
                missing.push(rank);
            }
        }
        drop(jobs);
        drop(ranks);
        drop(read);
        if missing.is_empty() {
            return Ok(());
        }
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut ranks = write.open_table(JOB_RANK)?;
            for key in &missing {
                ranks.insert(key.as_slice(), &[][..])?;
            }
        }
        write.commit()?;
        Ok(())
    }
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

fn needs_ticks(kind: u8, mutable: &MutableView) -> bool {
    concentrated_kind(kind) && mutable.tick_count > 0 && !mutable.truncated
}

fn first_missing_page(
    db: &redb::Database,
    kind: u8,
    id: B256,
    mutable: &MutableView,
) -> eyre::Result<u32> {
    let pages = mutable.tick_count.div_ceil(TICK_PAGE);
    let read = db.begin_read()?;
    let table = match read.open_table(TICK_PAGES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    for page in 0..pages {
        let key = tick_key(kind, id, page);
        match table.get(key.as_slice())? {
            None => return Ok(page),
            Some(value) => {
                let generation = tick_page_generation(value.value())?;
                if generation != mutable.generation {
                    return Ok(page);
                }
            }
        }
    }
    Ok(pages)
}

fn event_after_snapshot(
    table: &redb::Table<'_, &[u8], &[u8]>,
    hit: &WatchHit,
) -> eyre::Result<bool> {
    let key = entity_key(hit.kind, &hit.id);
    let Some(stored) = table.get(key.as_slice())? else {
        return Ok(false);
    };
    let view = decode_mutable(stored.value())?;
    Ok(hit.block > view.block)
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
        tracing::error!(
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
    tracing::warn!(
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

fn count_bytes(db: &redb::Database, def: TableDefinition<&[u8], &[u8]>) -> eyre::Result<u64> {
    let read = db.begin_read()?;
    let table = match read.open_table(def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    let mut n = 0_u64;
    for item in table.iter()? {
        item?;
        n += 1;
    }
    Ok(n)
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn job_key(stage: u8, kind: u8, id: &B256) -> [u8; 34] {
    let mut out = [0u8; 34];
    out[0] = stage;
    out[1] = kind;
    out[2..].copy_from_slice(id.as_slice());
    out
}

fn rank_key(stage: u8, priority: u64, kind: u8, id: &B256) -> [u8; 42] {
    let mut out = [0u8; 42];
    out[0] = stage;
    let inverted = u64::MAX - priority;
    out[1..9].copy_from_slice(&inverted.to_be_bytes());
    out[9] = kind;
    out[10..].copy_from_slice(id.as_slice());
    out
}

fn decode_rank(key: &[u8]) -> Option<(u8, u64, u8, B256)> {
    if key.len() != 42 {
        return None;
    }
    let inverted = u64::from_be_bytes(key[1..9].try_into().ok()?);
    Some((
        key[0],
        u64::MAX - inverted,
        key[9],
        B256::from_slice(&key[10..]),
    ))
}

fn priority_of(dirty: bool, top: U256) -> u64 {
    let mag = magnitude(top);
    if dirty {
        DIRTY_BAND | mag
    } else {
        mag
    }
}

/// Bit length in the high bits, so a longer `max_in` sorts first, then its top bits.
fn magnitude(value: U256) -> u64 {
    let bits = u256_bit_len(&value);
    if bits == 0 {
        return 0;
    }
    let shift = bits.saturating_sub(54);
    let shifted = if shift == 0 { value } else { value >> shift };
    let bytes = shifted.to_be_bytes::<32>();
    let mant = u64::from_be_bytes(bytes[24..32].try_into().unwrap());
    let mant = mant & ((1_u64 << 54) - 1);
    ((bits as u64) << 54) | mant
}

fn u256_bit_len(value: &U256) -> u32 {
    let bytes = value.to_be_bytes::<32>();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != 0 {
            return (32 - index as u32) * 8 - byte.leading_zeros();
        }
    }
    0
}

fn lease_job(job: &mut WorkJob, now: u64, lease_for: u64) -> bool {
    if job.lease_until > now || job.not_before > now {
        return false;
    }
    job.lease_until = now.saturating_add(lease_for);
    job.attempt = job.attempt.saturating_add(1);
    true
}

fn load_job(
    table: &redb::Table<'_, &[u8], &[u8]>,
    stage: u8,
    kind: u8,
    id: &B256,
) -> eyre::Result<Option<WorkJob>> {
    let key = job_key(stage, kind, id);
    let Some(stored) = table.get(key.as_slice())? else {
        return Ok(None);
    };
    Ok(Some(decode_job(key.as_slice(), stored.value())?))
}

fn put_job(
    jobs: &mut redb::Table<'_, &[u8], &[u8]>,
    ranks: &mut redb::Table<'_, &[u8], &[u8]>,
    job: &WorkJob,
) -> eyre::Result<()> {
    let key = job_key(job.stage, job.kind, &job.id);
    let bytes = encode_job(job);
    jobs.insert(key.as_slice(), bytes.as_slice())?;
    let rank = rank_key(job.stage, job.priority, job.kind, &job.id);
    ranks.insert(rank.as_slice(), &[][..])?;
    Ok(())
}

fn remove_rank(ranks: &mut redb::Table<'_, &[u8], &[u8]>, job: &WorkJob) -> eyre::Result<()> {
    let rank = rank_key(job.stage, job.priority, job.kind, &job.id);
    ranks.remove(rank.as_slice())?;
    Ok(())
}

fn ensure_rank(ranks: &mut redb::Table<'_, &[u8], &[u8]>, job: &WorkJob) -> eyre::Result<()> {
    let rank = rank_key(job.stage, job.priority, job.kind, &job.id);
    if ranks.get(rank.as_slice())?.is_none() {
        ranks.insert(rank.as_slice(), &[][..])?;
    }
    Ok(())
}

fn clear_job(
    jobs: &mut redb::Table<'_, &[u8], &[u8]>,
    ranks: &mut redb::Table<'_, &[u8], &[u8]>,
    stage: u8,
    kind: u8,
    id: &B256,
) -> eyre::Result<()> {
    let key = job_key(stage, kind, id);
    let Some(stored) = jobs.remove(key.as_slice())? else {
        return Ok(());
    };
    if let Ok(job) = decode_job(key.as_slice(), stored.value()) {
        remove_rank(ranks, &job)?;
    }
    Ok(())
}

fn stored_capacity(kind: u8, id: B256, state: &PoolState) -> Option<crate::fit::TradeCapacity> {
    let immutable = state.immutable.as_ref()?;
    let mutable = state.mutable.as_ref()?;
    let detail = join_detail(immutable, &mutable.body, &state.ticks, kind, id).ok()?;
    Some(crate::fit::trade_capacity(&detail))
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
async fn after_book<S: EntityStore>(store: &S, job: &WorkJob) -> eyre::Result<Option<WorkJob>> {
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

fn tick_key(kind: u8, id: B256, page: u32) -> [u8; 37] {
    let mut out = [0u8; 37];
    out[..33].copy_from_slice(&entity_key(kind, &id));
    out[33..].copy_from_slice(&page.to_be_bytes());
    out
}

fn clip_error(error: &str) -> String {
    error.chars().take(160).collect()
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
    log: &NormalizedLog,
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

fn topic_id(log: &NormalizedLog) -> Option<B256> {
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
        let catalog = Catalog::open(&dir.0).unwrap();
        (dir, catalog)
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
        let mint = NormalizedLog {
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
        let factory = NormalizedLog {
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
