//! Pool state, durable work, and the watch.
//!
//! Immutable rows are written once. Mutable rows, tick pages, and curves live
//! beside them in `entities.redb`. `work.redb` only remembers leases. A deleted
//! work file is rebuilt by scanning entities. The watch cursor and the dirty
//! flags commit together. A mutable snapshot clears its dirty flag in the same
//! transaction, and only when no newer event arrived during the read.

use crate::catalog::{entity_key, AERO, BALANCER_V2, BALANCER_V3, CURVE, METRIC, UNI_V4, VENUES};
use crate::detail::{
    concentrated_ladder, quote_ladder, tick_request, BitmapWord, Concentrated, CurveCoin,
    CurvePool, MetricBin, MetricPool, PoolDetail, Quote, TickWord, FLAG_CRYPTO, FLAG_FAILED,
    FLAG_STABLE, FLAG_TRUNCATED,
};
use crate::rpc::Rpc;
use crate::tick::{self, TickWordRead};
use crate::{
    decode_row, Catalog, EntityStore, RunOpts, RunReport, VenueRow, PANCAKE_V3, SUSHI_V3, UNI_V3,
    V3_FORK,
};
use alloy_primitives::{keccak256, Address, B256, U256};
use eyre::WrapErr;
use ipc::HistoryPeer;
use model::{BlockRecord, NormalizedLog};
use redb::{Durability, ReadableTable, TableDefinition};
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const IMMUTABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("immutable");
const MUTABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mutable");
const TICK_PAGES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("tick_pages");
const CURVES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("curves");
const DIRTY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("dirty");
const MISMATCH: TableDefinition<&[u8], &[u8]> = TableDefinition::new("immutable_mismatch");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");
const JOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("jobs");

const WATCH_KEY: &str = "watch_through";
/// Quote-math generation stored on a curve. A bump rebuilds every curve.
pub const CURVE_VERSION: u32 = 1;
pub const TICK_PAGE: u32 = 512;
pub const STAGE_IMMUTABLE: u8 = 1;
pub const STAGE_MUTABLE: u8 = 2;
pub const STAGE_TICKS: u8 = 3;
pub const STAGE_CURVE: u8 = 4;
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
    pub dirty: Option<Dirty>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub retry_mutable: bool,
    pub stage: Option<u8>,
}

/// What to enqueue after a mutable snapshot commits.
///
/// A head refresh with the same bitmap keeps tick pages and only refits the
/// curve. A mint or a changed bitmap has to read ticks again. A truncated
/// bitmap stays on the mutable stage.
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
    if let PoolDetail::Concentrated(pool) = detail {
        if pool.sqrt_price_x96.is_zero() {
            // Tag 0, reason 5: the pool was created and never initialized.
            return Ok(CurveRecord {
                kind,
                id,
                generation,
                version: CURVE_VERSION,
                payload: vec![0, 5],
            });
        }
    }
    let payload = if !is_canonical(detail) {
        let mut payload = vec![0];
        payload.push(skip_reason(detail));
        payload
    } else if let PoolDetail::Concentrated(pool) = detail {
        let ladder = concentrated_ladder(pool)?;
        let (immutable, mutable) = split_detail(detail)?;
        let mut payload = vec![2];
        push_bytes(&mut payload, &immutable);
        push_bytes(&mut payload, &mutable.body);
        push_ladder(&mut payload, &ladder.zero_for_one);
        push_ladder(&mut payload, &ladder.one_for_zero);
        payload
    } else {
        let (immutable, mutable) = split_detail(detail)?;
        let mut payload = vec![1];
        push_bytes(&mut payload, &immutable);
        push_bytes(&mut payload, &mutable.body);
        payload
    };
    Ok(CurveRecord {
        kind,
        id,
        generation,
        version: CURVE_VERSION,
        payload,
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
    let payload = curve.payload.as_slice();
    if payload.is_empty() {
        return Err("curve".into());
    }
    match payload[0] {
        0 => Err("not canonical".into()),
        1 => {
            let mut reader = Reader::new(&payload[1..]);
            let immutable = reader.bytes()?;
            let body = reader.bytes()?;
            reader.finish()?;
            join_detail(&immutable, &body, &[], curve.kind, curve.id)?
                .quote(token_in, token_out, amount_in)
        }
        2 => {
            let mut reader = Reader::new(&payload[1..]);
            let immutable = reader.bytes()?;
            let body = reader.bytes()?;
            let zero_for_one = reader.ladder()?;
            let one_for_zero = reader.ladder()?;
            reader.finish()?;
            let PoolDetail::Concentrated(pool) =
                join_detail(&immutable, &body, &[], curve.kind, curve.id)?
            else {
                return Err("ladder pool".into());
            };
            quote_ladder(
                &pool,
                &crate::detail::ConcentratedLadder {
                    zero_for_one,
                    one_for_zero,
                },
                token_in,
                token_out,
                amount_in,
            )
        }
        _ => Err("curve tag".into()),
    }
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
        Ok(summary)
    }

    pub fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(VENUES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        let key = entity_key(kind, &id);
        match table.get(key.as_slice())? {
            Some(value) => Ok(Some(decode_row(kind, id, value.value())?)),
            None => Ok(None),
        }
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
        write.set_durability(Durability::Immediate);
        {
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
            }
            meta.insert(WATCH_KEY, through)?;
        }
        write.commit()?;
        Ok(())
    }

    pub fn put_immutable(&self, rows: &[ImmutableRow]) -> eyre::Result<Vec<ImmutablePut>> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate);
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
    /// The dirty flag is removed only when it does not name a newer block.
    pub fn put_mutable(&self, row: &MutableWrite) -> eyre::Result<MutableStored> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate);
        let stored = {
            let mut mutable_table = write.open_table(MUTABLE)?;
            let mut ticks = write.open_table(TICK_PAGES)?;
            let mut curves = write.open_table(CURVES)?;
            let mut dirty_table = write.open_table(DIRTY)?;
            let key = entity_key(row.kind, &row.id);
            let previous = match mutable_table.get(key.as_slice())? {
                Some(value) => Some(decode_mutable(value.value())?),
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
            MutableStored {
                generation,
                dirty_cleared,
                kept_ticks,
                bitmap_changed,
            }
        };
        write.commit()?;
        Ok(stored)
    }

    pub fn put_tick_pages(&self, pages: &[TickPage]) -> eyre::Result<()> {
        if pages.is_empty() {
            return Ok(());
        }
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate);
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
        write.set_durability(Durability::Immediate);
        {
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
            let bytes = encode_curve(curve);
            curves.insert(key.as_slice(), bytes.as_slice())?;
        }
        write.commit()?;
        Ok(())
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
        write.set_durability(Durability::Immediate);
        let jobs = {
            let mut table = write.open_table(JOBS)?;
            let mut picked = Vec::new();
            let start = job_key(stage, 0, &B256::ZERO);
            let end = job_key(stage.saturating_add(1), 0, &B256::ZERO);
            for item in table.range::<&[u8]>(start.as_slice()..end.as_slice())? {
                if picked.len() == limit as usize {
                    break;
                }
                let (key, value) = item?;
                let mut job = decode_job(key.value(), value.value())?;
                if job.lease_until > now || job.not_before > now {
                    continue;
                }
                job.lease_until = now.saturating_add(lease_for);
                job.attempt = job.attempt.saturating_add(1);
                let bytes = encode_job(&job);
                picked.push((key.value().to_vec(), job, bytes));
            }
            let mut jobs = Vec::with_capacity(picked.len());
            for (key, job, bytes) in picked {
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
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate);
        {
            let mut table = write.open_table(JOBS)?;
            for job in jobs {
                let key = job_key(job.stage, job.kind, &job.id);
                if let Some(stored) = table.get(key.as_slice())? {
                    let existing = decode_job(key.as_slice(), stored.value())?;
                    if existing.lease_until > now {
                        continue;
                    }
                }
                let bytes = encode_job(job);
                table.insert(key.as_slice(), bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    pub fn finish_work(&self, done: &[(u8, u8, B256)], failed: &[WorkFail]) -> eyre::Result<()> {
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate);
        {
            let mut table = write.open_table(JOBS)?;
            for (stage, kind, id) in done {
                let key = job_key(*stage, *kind, id);
                table.remove(key.as_slice())?;
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
                let bytes = encode_job(&job);
                table.insert(key.as_slice(), bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    fn reconcile_rows(&self, rows: &[VenueRow], now: u64) -> eyre::Result<u64> {
        let mut jobs = Vec::new();
        for row in rows {
            let state = self.read_state(row.kind, row.id)?;
            if state.immutable.is_none() {
                jobs.push(blank_job(STAGE_IMMUTABLE, row.kind, row.id));
                continue;
            }
            let Some(mutable) = state.mutable else {
                jobs.push(blank_job(STAGE_MUTABLE, row.kind, row.id));
                continue;
            };
            if state.dirty.is_some() || mutable.truncated {
                let mut job = blank_job(STAGE_MUTABLE, row.kind, row.id);
                job.generation = mutable.generation;
                job.class = state.dirty.map(|dirty| dirty.class).unwrap_or(CLASS_NONE);
                jobs.push(job);
                continue;
            }
            if needs_ticks(row.kind, &mutable) {
                let cursor = first_missing_page(&self.entities, row.kind, row.id, &mutable)?;
                let pages = mutable.tick_count.div_ceil(TICK_PAGE);
                if cursor < pages {
                    let mut job = blank_job(STAGE_TICKS, row.kind, row.id);
                    job.generation = mutable.generation;
                    job.tick_cursor = i32::try_from(cursor).unwrap_or(i32::MAX);
                    jobs.push(job);
                    continue;
                }
            }
            let stale = match &state.curve {
                Some(curve) => {
                    curve.generation != mutable.generation || curve.version != CURVE_VERSION
                }
                None => true,
            };
            if stale {
                let mut job = blank_job(STAGE_CURVE, row.kind, row.id);
                job.generation = mutable.generation;
                jobs.push(job);
            }
        }
        let before = count_jobs(&self.work)?;
        self.enqueue_work(&jobs, now)?;
        let after = count_jobs(&self.work)?;
        Ok(after.saturating_sub(before))
    }
}

/// One pass of the state pipeline. The caller loops until shutdown.
///
/// A pass reconciles only when asked. Each pass leases a single stage.
pub async fn run<S, P>(
    store: &S,
    peer: Option<&P>,
    opts: &RunOpts,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<RunReport>
where
    S: EntityStore + Clone + 'static,
    P: HistoryPeer + Clone + 'static,
{
    let mut report = RunReport::default();
    if *stop.borrow() {
        return Ok(report);
    }
    let inserted = store.reconcile(unix_now()).await?;
    report.notes.push(format!("reconcile {inserted}"));
    let urls = if opts.rpcs.is_empty() {
        crate::rpc::default_call_rpcs()
    } else {
        opts.rpcs.clone()
    };
    // reqwest's blocking client builds a shell runtime. Doing that on a
    // worker panics in debug. The blocking pool is allowed to block.
    let rpc = tokio::task::spawn_blocking(move || Rpc::new(urls))
        .await
        .wrap_err("rpc client task")??;
    let block = match opts.pin_block {
        Some(pin) => pin,
        None => match store.header_cursor().await? {
            cursor if cursor > 0 => cursor - 1,
            _ => {
                let rpc = rpc.clone();
                tokio::task::spawn_blocking(move || rpc.block_number())
                    .await
                    .wrap_err("block number task")??
            }
        },
    };
    tracing::info!(block, "state block");
    let give_up_after = if opts.until_idle { 4 } else { 0 };
    let mut quiet = 0_u32;
    let mut batches = 0_u64;
    loop {
        if *stop.borrow() {
            return Ok(report);
        }
        if let Some(peer) = peer {
            match watch_once(store, peer).await {
                Ok(note) => {
                    if let Some(note) = note {
                        tracing::info!(%note, "watch");
                    }
                }
                Err(err) => {
                    let note = format!("watch: {err:#}");
                    tracing::error!(%note);
                    report.notes.push(note);
                }
            }
        }
        if *stop.borrow() {
            return Ok(report);
        }
        let mut notes = Vec::new();
        let worked = drive_one(store, &rpc, block, unix_now(), give_up_after, &mut notes).await?;
        report.notes.extend(notes);
        if worked {
            quiet = 0;
            batches += 1;
            if batches.is_multiple_of(25) {
                tracing::info!(batches, "state progress");
            }
            continue;
        }
        if opts.until_idle {
            quiet += 1;
            if quiet >= 2 {
                report.notes.push("state idle".into());
                tracing::info!(batches, "state idle");
                return Ok(report);
            }
            tokio::select! {
                _ = stop.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(RETRY_SECS + 5)) => {}
            }
            continue;
        }
        sleep_or_stop(&mut stop).await;
    }
}

async fn sleep_or_stop(stop: &mut tokio::sync::watch::Receiver<bool>) {
    tokio::select! {
        _ = stop.changed() => {}
        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
    }
}

async fn drive_one<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    block: u64,
    now: u64,
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<bool> {
    for stage in [STAGE_IMMUTABLE, STAGE_MUTABLE, STAGE_TICKS, STAGE_CURVE] {
        let jobs = store
            .claim_work(stage, DETAIL_BATCH, now, LEASE_SECS)
            .await?;
        if jobs.is_empty() {
            continue;
        }
        let started = std::time::Instant::now();
        match stage {
            STAGE_IMMUTABLE | STAGE_MUTABLE => {
                drive_details(store, rpc, block, stage, &jobs, give_up_after, notes).await?
            }
            STAGE_TICKS => drive_ticks(store, rpc, &jobs, now, give_up_after, notes).await?,
            STAGE_CURVE => drive_curves(store, &jobs, give_up_after, notes).await?,
            _ => {}
        }
        tracing::info!(
            stage,
            jobs = jobs.len(),
            ms = started.elapsed().as_millis() as u64,
            "state batch"
        );
        return Ok(true);
    }
    Ok(false)
}

async fn drive_details<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    block: u64,
    stage: u8,
    jobs: &[WorkJob],
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<()> {
    let mut rows = Vec::new();
    let mut missing = Vec::new();
    for job in jobs {
        match store.read_venue(job.kind, job.id).await? {
            Some(row) => rows.push(row),
            None => missing.push((job.stage, job.kind, job.id)),
        }
    }
    if !missing.is_empty() {
        store.finish_work(&missing, &[]).await?;
    }
    if rows.is_empty() {
        return Ok(());
    }
    let mut done = Vec::new();
    let mut failed = Vec::new();
    let mut follow = Vec::new();
    let rpc = rpc.clone();
    let queries = rows.clone();
    let loaded = match tokio::task::spawn_blocking(move || {
        crate::detail::load_details_at(&rpc, &queries, Some(block))
    })
    .await
    {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(err)) => {
            let error = crate::rpc::redact_secret(&format!("{err:#}"));
            for row in &rows {
                let fallback = blank_job(stage, row.kind, row.id);
                let job = job_for(jobs, row.kind, row.id).unwrap_or(&fallback);
                keep_or_drop(
                    job,
                    error.clone(),
                    give_up_after,
                    &mut done,
                    &mut failed,
                    notes,
                );
            }
            store.finish_work(&done, &failed).await?;
            return Ok(());
        }
        Err(err) => return Err(eyre::eyre!("detail task: {err}")),
    };
    if loaded.len() != rows.len() {
        return Err(eyre::eyre!("detail lens row count"));
    }
    for (row, detail) in rows.iter().zip(loaded) {
        if detail_flags(&detail) & FLAG_FAILED != 0 {
            let fallback = blank_job(stage, row.kind, row.id);
            let job = job_for(jobs, row.kind, row.id).unwrap_or(&fallback);
            keep_or_drop(
                job,
                "pool read failed".into(),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
            continue;
        }
        let (immutable, mut mutable) = match split_detail(&detail) {
            Ok(parts) => parts,
            Err(err) => {
                let fallback = blank_job(stage, row.kind, row.id);
                let job = job_for(jobs, row.kind, row.id).unwrap_or(&fallback);
                keep_or_drop(job, err, give_up_after, &mut done, &mut failed, notes);
                continue;
            }
        };
        mutable.block = block;
        if stage == STAGE_IMMUTABLE {
            store
                .put_immutable(&[ImmutableRow {
                    kind: row.kind,
                    id: row.id,
                    body: immutable,
                }])
                .await?;
            mutable.class = CLASS_NONE;
        } else {
            let state = store.read_state(row.kind, row.id).await?;
            mutable.class = state.dirty.map(|dirty| dirty.class).unwrap_or(CLASS_HEAD);
        }
        let stored = store.put_mutable(&mutable).await?;
        let canonical = is_canonical(&detail);
        let plan = plan_follow_up(
            mutable.class,
            stored.bitmap_changed && !stored.kept_ticks,
            mutable.truncated,
            mutable.tick_count,
            canonical,
        );
        if plan.retry_mutable {
            follow.push(blank_job(STAGE_MUTABLE, row.kind, row.id));
        } else if let Some(next) = plan.stage {
            let mut job = blank_job(next, row.kind, row.id);
            job.generation = stored.generation;
            follow.push(job);
            if next == STAGE_CURVE && !canonical {
                if let Ok(curve) = derive_curve(&detail, stored.generation) {
                    if store.put_curve(&curve).await.is_ok() {
                        follow.pop();
                    }
                }
            }
        }
        done.push((stage, row.kind, row.id));
    }
    store.enqueue_work(&follow, unix_now()).await?;
    store.finish_work(&done, &failed).await?;
    Ok(())
}

async fn drive_ticks<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    jobs: &[WorkJob],
    now: u64,
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<()> {
    let mut requests = Vec::new();
    let mut metas = Vec::new();
    let mut done = Vec::new();
    let mut failed = Vec::new();
    let mut follow = Vec::new();
    for job in jobs {
        let state = store.read_state(job.kind, job.id).await?;
        let Some(immutable) = state.immutable else {
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        let Some(mutable) = state.mutable else {
            follow.push(blank_job(STAGE_MUTABLE, job.kind, job.id));
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        if job.generation != 0 && job.generation != mutable.generation {
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        let detail = match join_detail(&immutable, &mutable.body, &[], job.kind, job.id) {
            Ok(detail) => detail,
            Err(err) => {
                keep_or_drop(job, err, give_up_after, &mut done, &mut failed, notes);
                continue;
            }
        };
        let PoolDetail::Concentrated(pool) = detail else {
            follow.push(curve_job(job.kind, job.id, mutable.generation));
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        let ticks = pool.initialized_ticks();
        let page = u32::try_from(job.tick_cursor.max(0)).unwrap_or(0);
        let start = page.saturating_mul(TICK_PAGE) as usize;
        if start >= ticks.len() {
            follow.push(curve_job(job.kind, job.id, mutable.generation));
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        let end = (start + TICK_PAGE as usize).min(ticks.len());
        requests.push(tick_request(&pool, ticks[start..end].to_vec()));
        metas.push((
            job.clone(),
            mutable.generation,
            mutable.block,
            page,
            end < ticks.len(),
        ));
    }
    if requests.is_empty() {
        store.enqueue_work(&follow, now).await?;
        store.finish_work(&done, &failed).await?;
        return Ok(());
    }
    let mut groups: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (index, meta) in metas.iter().enumerate() {
        groups.entry(meta.2).or_default().push(index);
    }
    let mut words_by_index = vec![Vec::new(); metas.len()];
    let mut failed_by_index = vec![false; metas.len()];
    let mut errors_by_index = vec![None; metas.len()];
    for (block, indexes) in groups {
        let batch: Vec<_> = indexes
            .iter()
            .map(|index| requests[*index].clone())
            .collect();
        let rpc = rpc.clone();
        let loaded =
            match tokio::task::spawn_blocking(move || tick::load_ticks(&rpc, &batch, Some(block)))
                .await
            {
                Ok(Ok(loaded)) => loaded,
                Ok(Err(err)) => {
                    let error = crate::rpc::redact_secret(&format!("{err:#}"));
                    for index in &indexes {
                        errors_by_index[*index] = Some(error.clone());
                    }
                    continue;
                }
                Err(err) => return Err(eyre::eyre!("tick task: {err}")),
            };
        if loaded.ticks.len() != indexes.len() || loaded.failed.len() != indexes.len() {
            return Err(eyre::eyre!("tick lens row count"));
        }
        for (slot, index) in indexes.into_iter().enumerate() {
            words_by_index[index] = loaded.ticks[slot].clone();
            failed_by_index[index] = loaded.failed[slot];
        }
    }
    for (index, (job, generation, _block, page, more)) in metas.into_iter().enumerate() {
        let words = std::mem::take(&mut words_by_index[index]);
        if let Some(error) = errors_by_index[index].clone() {
            keep_or_drop(&job, error, give_up_after, &mut done, &mut failed, notes);
            continue;
        }
        let failed_read = failed_by_index[index];
        if failed_read {
            keep_or_drop(
                &job,
                "tick read failed".into(),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
            continue;
        }
        let ticks = words
            .into_iter()
            .map(|TickWordRead { tick, word }| {
                let (liquidity_gross, liquidity_net) = unpack_stored(word);
                TickWord {
                    tick,
                    liquidity_gross,
                    liquidity_net,
                }
            })
            .collect();
        store
            .put_tick_pages(&[TickPage {
                kind: job.kind,
                id: job.id,
                page,
                generation,
                ticks,
            }])
            .await?;
        if more {
            let mut again = job.clone();
            again.lease_until = 0;
            again.not_before = 0;
            again.tick_cursor = page.saturating_add(1) as i32;
            again.generation = generation;
            follow.push(again);
        } else {
            follow.push(curve_job(job.kind, job.id, generation));
        }
        done.push((job.stage, job.kind, job.id));
    }
    store.enqueue_work(&follow, now).await?;
    store.finish_work(&done, &failed).await?;
    Ok(())
}

async fn drive_curves<S: EntityStore>(
    store: &S,
    jobs: &[WorkJob],
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<()> {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for job in jobs {
        let state = store.read_state(job.kind, job.id).await?;
        let (Some(immutable), Some(mutable)) = (state.immutable, state.mutable) else {
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        if job.generation != 0 && job.generation != mutable.generation {
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        let detail = match join_detail(&immutable, &mutable.body, &state.ticks, job.kind, job.id) {
            Ok(detail) => detail,
            Err(err) => {
                keep_or_drop(job, err, give_up_after, &mut done, &mut failed, notes);
                continue;
            }
        };
        let curve = match derive_curve(&detail, mutable.generation) {
            Ok(curve) => curve,
            Err(err) => {
                keep_or_drop(job, err, give_up_after, &mut done, &mut failed, notes);
                continue;
            }
        };
        if let Err(err) = store.put_curve(&curve).await {
            keep_or_drop(
                job,
                err.to_string(),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
            continue;
        }
        done.push((job.stage, job.kind, job.id));
    }
    store.finish_work(&done, &failed).await?;
    Ok(())
}

async fn watch_once<S: EntityStore, P: HistoryPeer>(
    store: &S,
    peer: &P,
) -> eyre::Result<Option<String>> {
    let head = store.header_cursor().await?;
    if head == 0 {
        return Ok(None);
    }
    if store.watch_through().await?.is_none() {
        store.apply_watch(head, &[]).await?;
        return Ok(Some(format!("watch armed at {head}")));
    }
    let from = store.watch_through().await?.unwrap_or(0);
    if head <= from {
        return Ok(None);
    }
    let count = (head - from).min(8);
    let blocks = peer.chunk(from, count).await?;
    if blocks.is_empty() {
        return Ok(None);
    }
    if !contiguous(&blocks, from) {
        return Err(eyre::eyre!("watch chunk did not start at {from}"));
    }
    let mut hits = Vec::new();
    for block in &blocks {
        let logs = corpus::logs_in_block(block)?;
        for log in logs {
            if let Some(hit) = watch_hit(store, &log).await? {
                hits.push(hit);
            }
        }
    }
    let through = from + blocks.len() as u64;
    store.apply_watch(through, &hits).await?;
    Ok(Some(format!("watch {from}..{through} hits {}", hits.len())))
}

async fn watch_hit<S: EntityStore>(
    store: &S,
    log: &NormalizedLog,
) -> eyre::Result<Option<WatchHit>> {
    let Some(&topic) = log.topics.first() else {
        return Ok(None);
    };
    let known = crate::decode::known();
    if log.address == known.pool_manager || log.address == known.bal_v2 {
        let (kind, head, other, _other_class) = if log.address == known.pool_manager {
            (UNI_V4, TOPIC_V4_SWAP, TOPIC_V4_LIQUIDITY, CLASS_TICKS)
        } else {
            (BALANCER_V2, TOPIC_BAL_SWAP, TOPIC_BAL_BALANCE, CLASS_HEAD)
        };
        if topic != head && topic != other {
            return Ok(None);
        }
        let Some(id) = topic_id(log) else {
            return Ok(None);
        };
        let present = store.read_venue(kind, id).await?.is_some();
        return Ok(classify_log(
            log,
            |asked_kind, asked_id| asked_kind == kind && asked_id == id && present,
            |_| None,
        ));
    }
    if log.address == known.bal_v3 && topic == TOPIC_BAL3_SWAP {
        let Some(id) = topic_id(log) else {
            return Ok(None);
        };
        let present = store.read_venue(BALANCER_V3, id).await?.is_some();
        return Ok(classify_log(
            log,
            |asked_kind, asked_id| asked_kind == BALANCER_V3 && asked_id == id && present,
            |_| None,
        ));
    }
    if log.address == known.bal_v3 {
        return Ok(None);
    }
    let Some((kind, id)) = find_address(store, log.address).await? else {
        return Ok(None);
    };
    Ok(classify_log(log, |_, _| false, |_| Some((kind, id))))
}

async fn find_address<S: EntityStore>(
    store: &S,
    address: Address,
) -> eyre::Result<Option<(u8, B256)>> {
    let id = VenueRow::address_id(address);
    for kind in 1..=METRIC {
        if !(concentrated_kind(kind)
            || matches!(kind, 1 | 2 | 3 | 4 | AERO | CURVE | METRIC | BALANCER_V3))
        {
            continue;
        }
        if store.read_venue(kind, id).await?.is_some() {
            return Ok(Some((kind, id)));
        }
    }
    Ok(None)
}

fn contiguous(blocks: &[BlockRecord], from: u64) -> bool {
    blocks
        .iter()
        .enumerate()
        .all(|(index, block)| block.number == from + index as u64)
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
        error: String::new(),
    }
}

fn curve_job(kind: u8, id: B256, generation: u64) -> WorkJob {
    let mut job = blank_job(STAGE_CURVE, kind, id);
    job.generation = generation;
    job
}

fn keep_or_drop(
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

fn count_jobs(db: &redb::Database) -> eyre::Result<u64> {
    let read = db.begin_read()?;
    let table = match read.open_table(JOBS) {
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

fn unix_now() -> u64 {
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

fn skip_reason(detail: &PoolDetail) -> u8 {
    match detail {
        PoolDetail::Metric(_) => 1,
        PoolDetail::Curve(_) => 2,
        PoolDetail::Balancer(_) => 3,
        _ => 4,
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn rest(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if self.rest() < len {
            return Err("short".into());
        }
        let start = self.at;
        self.at += len;
        Ok(&self.bytes[start..self.at])
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, String> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u128(&mut self) -> Result<u128, String> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    fn i128(&mut self) -> Result<i128, String> {
        Ok(i128::from_le_bytes(self.take(16)?.try_into().unwrap()))
    }

    fn addr(&mut self) -> Result<Address, String> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn hash(&mut self) -> Result<B256, String> {
        Ok(B256::from_slice(self.take(32)?))
    }

    fn word(&mut self) -> Result<U256, String> {
        Ok(U256::from_be_slice(self.take(32)?))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, String> {
        let len = self.u32()? as usize;
        Ok(self.take(len)?.to_vec())
    }

    fn ladder(&mut self) -> Result<Vec<crate::detail::LadderStep>, String> {
        let n = self.u32()? as usize;
        let mut steps = Vec::with_capacity(n);
        for _ in 0..n {
            steps.push(crate::detail::LadderStep {
                liquidity: self.u128()?,
                sqrt_start: self.word()?,
                sqrt_end: self.word()?,
                tick_end: self.i32()?,
                input: self.word()?,
                output: self.word()?,
                fee: self.word()?,
            });
        }
        Ok(steps)
    }

    fn finish(&self) -> Result<(), String> {
        if self.at == self.bytes.len() {
            Ok(())
        } else {
            Err("trailing".into())
        }
    }
}

fn push_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_u128(out: &mut Vec<u8>, value: u128) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push_addr(out: &mut Vec<u8>, value: Address) {
    out.extend_from_slice(value.as_slice());
}

fn push_word(out: &mut Vec<u8>, value: U256) {
    out.extend_from_slice(&value.to_be_bytes::<32>());
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_u32(out, u32::try_from(bytes.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(bytes);
}

fn push_option_u32(out: &mut Vec<u8>, value: Option<u32>) {
    match value {
        Some(value) => {
            push_u8(out, 1);
            push_u32(out, value);
        }
        None => push_u8(out, 0),
    }
}

fn push_option_word(out: &mut Vec<u8>, value: Option<U256>) {
    match value {
        Some(value) => {
            push_u8(out, 1);
            push_word(out, value);
        }
        None => push_u8(out, 0),
    }
}

fn push_ladder(out: &mut Vec<u8>, steps: &[crate::detail::LadderStep]) {
    push_u32(out, steps.len() as u32);
    for step in steps {
        push_u128(out, step.liquidity);
        push_word(out, step.sqrt_start);
        push_word(out, step.sqrt_end);
        push_i32(out, step.tick_end);
        push_word(out, step.input);
        push_word(out, step.output);
        push_word(out, step.fee);
    }
}

fn encode_product_immutable(pool: &crate::detail::ConstantProduct) -> Vec<u8> {
    let mut out = vec![1];
    push_addr(&mut out, pool.pair);
    push_addr(&mut out, pool.token0);
    push_addr(&mut out, pool.token1);
    out
}

fn encode_product_mutable(pool: &crate::detail::ConstantProduct) -> Vec<u8> {
    let mut out = Vec::new();
    push_word(&mut out, pool.reserve0);
    push_word(&mut out, pool.reserve1);
    push_option_u32(&mut out, pool.fee_pips);
    push_u64(&mut out, pool.timestamp);
    push_u64(&mut out, pool.flags);
    out
}

fn decode_product(immutable: &[u8], body: &[u8]) -> Result<crate::detail::ConstantProduct, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 1 {
        return Err("product".into());
    }
    let pair = header.addr()?;
    let token0 = header.addr()?;
    let token1 = header.addr()?;
    header.finish()?;
    let mut body = Reader::new(body);
    let pool = crate::detail::ConstantProduct {
        kind: 0,
        id: B256::ZERO,
        pair,
        token0,
        token1,
        reserve0: body.word()?,
        reserve1: body.word()?,
        fee_pips: if body.u8()? == 1 {
            Some(body.u32()?)
        } else {
            None
        },
        timestamp: body.u64()?,
        flags: body.u64()?,
    };
    body.finish()?;
    Ok(pool)
}

fn encode_concentrated_immutable(pool: &Concentrated) -> Vec<u8> {
    let mut out = vec![2];
    push_u8(&mut out, pool.kind);
    out.extend_from_slice(pool.id.as_slice());
    push_addr(&mut out, pool.home);
    push_addr(&mut out, pool.hooks);
    push_addr(&mut out, pool.token0);
    push_addr(&mut out, pool.token1);
    push_option_u32(&mut out, pool.fee_pips);
    push_i32(&mut out, pool.tick_spacing);
    out
}

fn encode_concentrated_mutable(pool: &Concentrated) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, pool.protocol_fee);
    push_word(&mut out, pool.sqrt_price_x96);
    push_i32(&mut out, pool.tick);
    push_u128(&mut out, pool.liquidity);
    push_u64(&mut out, pool.flags);
    match pool.next_scan {
        Some(value) => {
            push_u8(&mut out, 1);
            push_i32(&mut out, value);
        }
        None => push_u8(&mut out, 0),
    }
    push_u32(&mut out, pool.bitmaps.len() as u32);
    for word in &pool.bitmaps {
        push_i32(&mut out, i32::from(word.word_pos));
        push_word(&mut out, word.bits);
    }
    out
}

fn decode_concentrated(immutable: &[u8], body: &[u8]) -> Result<Concentrated, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 2 {
        return Err("concentrated".into());
    }
    let kind = header.u8()?;
    let id = header.hash()?;
    let pool = Concentrated {
        kind,
        id,
        home: header.addr()?,
        hooks: header.addr()?,
        token0: header.addr()?,
        token1: header.addr()?,
        fee_pips: if header.u8()? == 1 {
            Some(header.u32()?)
        } else {
            None
        },
        tick_spacing: header.i32()?,
        protocol_fee: 0,
        sqrt_price_x96: U256::ZERO,
        tick: 0,
        liquidity: 0,
        next_scan: None,
        bitmaps: Vec::new(),
        ticks: Vec::new(),
        flags: 0,
    };
    header.finish()?;
    let mut body = Reader::new(body);
    let protocol_fee = body.u32()?;
    let sqrt_price_x96 = body.word()?;
    let tick = body.i32()?;
    let liquidity = body.u128()?;
    let flags = body.u64()?;
    let next_scan = if body.u8()? == 1 {
        Some(body.i32()?)
    } else {
        None
    };
    let count = body.u32()? as usize;
    let mut bitmaps = Vec::with_capacity(count);
    for _ in 0..count {
        let word_pos = i16::try_from(body.i32()?).map_err(|_| "word pos")?;
        bitmaps.push(BitmapWord {
            word_pos,
            bits: body.word()?,
        });
    }
    body.finish()?;
    Ok(Concentrated {
        protocol_fee,
        sqrt_price_x96,
        tick,
        liquidity,
        flags,
        next_scan,
        bitmaps,
        ..pool
    })
}

fn encode_balancer_immutable(pool: &crate::detail::BalancerPool) -> Vec<u8> {
    let mut out = vec![3];
    push_u8(&mut out, pool.kind);
    out.extend_from_slice(pool.id.as_slice());
    push_addr(&mut out, pool.vault);
    push_addr(&mut out, pool.pool);
    push_u32(&mut out, pool.tokens.len() as u32);
    for token in &pool.tokens {
        push_addr(&mut out, *token);
    }
    out
}

fn encode_balancer_mutable(pool: &crate::detail::BalancerPool) -> Vec<u8> {
    let mut out = Vec::new();
    push_option_word(&mut out, pool.fee);
    push_word(&mut out, pool.amp);
    push_word(&mut out, pool.amp_precision);
    push_u64(&mut out, pool.flags);
    push_u32(&mut out, pool.balances.len() as u32);
    for balance in &pool.balances {
        push_word(&mut out, *balance);
    }
    push_u32(&mut out, pool.weights.len() as u32);
    for weight in &pool.weights {
        push_word(&mut out, *weight);
    }
    out
}

fn decode_balancer(immutable: &[u8], body: &[u8]) -> Result<crate::detail::BalancerPool, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 3 {
        return Err("balancer".into());
    }
    let kind = header.u8()?;
    let id = header.hash()?;
    let vault = header.addr()?;
    let pool = header.addr()?;
    let n = header.u32()? as usize;
    let mut tokens = Vec::with_capacity(n);
    for _ in 0..n {
        tokens.push(header.addr()?);
    }
    header.finish()?;
    let mut body = Reader::new(body);
    let fee = if body.u8()? == 1 {
        Some(body.word()?)
    } else {
        None
    };
    let amp = body.word()?;
    let amp_precision = body.word()?;
    let flags = body.u64()?;
    let balances_n = body.u32()? as usize;
    let mut balances = Vec::with_capacity(balances_n);
    for _ in 0..balances_n {
        balances.push(body.word()?);
    }
    let weights_n = body.u32()? as usize;
    let mut weights = Vec::with_capacity(weights_n);
    for _ in 0..weights_n {
        weights.push(body.word()?);
    }
    body.finish()?;
    Ok(crate::detail::BalancerPool {
        kind,
        id,
        vault,
        pool,
        fee,
        amp,
        amp_precision,
        tokens,
        balances,
        weights,
        flags,
    })
}

fn encode_curve_immutable(pool: &CurvePool) -> Vec<u8> {
    let mut out = vec![4];
    push_u8(&mut out, pool.kind);
    out.extend_from_slice(pool.id.as_slice());
    push_addr(&mut out, pool.pool);
    push_u8(&mut out, u8::from(pool.coins_indexed_by_uint));
    push_u32(&mut out, pool.coins.len() as u32);
    for coin in &pool.coins {
        push_addr(&mut out, coin.token);
    }
    out
}

fn encode_curve_mutable(pool: &CurvePool) -> Vec<u8> {
    let mut out = Vec::new();
    push_word(&mut out, pool.amp);
    push_option_word(&mut out, pool.fee);
    push_word(&mut out, pool.gamma);
    push_word(&mut out, pool.d);
    push_word(&mut out, pool.mid_fee);
    push_word(&mut out, pool.out_fee);
    push_word(&mut out, pool.fee_gamma);
    push_u64(&mut out, pool.flags);
    for coin in &pool.coins {
        push_word(&mut out, coin.balance);
        push_word(&mut out, coin.rate);
        push_word(&mut out, coin.price_scale);
    }
    out
}

fn decode_curve(immutable: &[u8], body: &[u8]) -> Result<CurvePool, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 4 {
        return Err("curve".into());
    }
    let kind = header.u8()?;
    let id = header.hash()?;
    let pool = header.addr()?;
    let indexed = header.u8()? != 0;
    let n = header.u32()? as usize;
    let mut tokens = Vec::with_capacity(n);
    for _ in 0..n {
        tokens.push(header.addr()?);
    }
    header.finish()?;
    let mut body = Reader::new(body);
    let amp = body.word()?;
    let fee = if body.u8()? == 1 {
        Some(body.word()?)
    } else {
        None
    };
    let gamma = body.word()?;
    let d = body.word()?;
    let mid_fee = body.word()?;
    let out_fee = body.word()?;
    let fee_gamma = body.word()?;
    let flags = body.u64()?;
    let mut coins = Vec::with_capacity(n);
    for token in tokens {
        coins.push(CurveCoin {
            token,
            balance: body.word()?,
            rate: body.word()?,
            price_scale: body.word()?,
        });
    }
    body.finish()?;
    Ok(CurvePool {
        kind,
        id,
        pool,
        amp,
        fee,
        gamma,
        d,
        mid_fee,
        out_fee,
        fee_gamma,
        coins_indexed_by_uint: indexed,
        coins,
        flags,
    })
}

fn encode_metric_immutable(pool: &MetricPool) -> Vec<u8> {
    let mut out = vec![5];
    push_u8(&mut out, pool.kind);
    out.extend_from_slice(pool.id.as_slice());
    push_addr(&mut out, pool.pool);
    push_addr(&mut out, pool.token0);
    push_addr(&mut out, pool.token1);
    out
}

fn encode_metric_mutable(pool: &MetricPool) -> Vec<u8> {
    let mut out = Vec::new();
    push_u8(&mut out, pool.pause_level);
    push_i32(&mut out, pool.bin_index);
    push_u128(&mut out, pool.position);
    push_i32(&mut out, pool.distance_e6);
    push_u32(&mut out, pool.spread_fee_e6);
    push_u32(&mut out, pool.notional_fee_e8);
    out.extend_from_slice(&pool.asymmetry_x8.to_le_bytes());
    push_word(&mut out, pool.scale0);
    push_word(&mut out, pool.scale1);
    push_i32(&mut out, pool.lowest_bin);
    push_i32(&mut out, pool.highest_bin);
    push_word(&mut out, pool.reference_x64);
    push_addr(&mut out, pool.provider);
    push_u64(&mut out, pool.flags);
    match pool.next_scan {
        Some(value) => {
            push_u8(&mut out, 1);
            push_i32(&mut out, value);
        }
        None => push_u8(&mut out, 0),
    }
    push_u32(&mut out, pool.bins.len() as u32);
    for bin in &pool.bins {
        push_i32(&mut out, bin.index);
        push_u128(&mut out, bin.token0_scaled);
        push_u128(&mut out, bin.token1_scaled);
        out.extend_from_slice(&bin.length_e6.to_le_bytes());
        out.extend_from_slice(&bin.add_fee_buy_e6.to_le_bytes());
        out.extend_from_slice(&bin.add_fee_sell_e6.to_le_bytes());
    }
    out
}

fn decode_metric(immutable: &[u8], body: &[u8]) -> Result<MetricPool, String> {
    let mut header = Reader::new(immutable);
    if header.u8()? != 5 {
        return Err("metric".into());
    }
    let kind = header.u8()?;
    let id = header.hash()?;
    let pool = header.addr()?;
    let token0 = header.addr()?;
    let token1 = header.addr()?;
    header.finish()?;
    let mut body = Reader::new(body);
    let pause_level = body.u8()?;
    let bin_index = body.i32()?;
    let position = body.u128()?;
    let distance_e6 = body.i32()?;
    let spread_fee_e6 = body.u32()?;
    let notional_fee_e8 = body.u32()?;
    let asymmetry_x8 = u16::from_le_bytes(body.take(2)?.try_into().unwrap());
    let scale0 = body.word()?;
    let scale1 = body.word()?;
    let lowest_bin = body.i32()?;
    let highest_bin = body.i32()?;
    let reference_x64 = body.word()?;
    let provider = body.addr()?;
    let flags = body.u64()?;
    let next_scan = if body.u8()? == 1 {
        Some(body.i32()?)
    } else {
        None
    };
    let n = body.u32()? as usize;
    let mut bins = Vec::with_capacity(n);
    for _ in 0..n {
        bins.push(MetricBin {
            index: body.i32()?,
            token0_scaled: body.u128()?,
            token1_scaled: body.u128()?,
            length_e6: u16::from_le_bytes(body.take(2)?.try_into().unwrap()),
            add_fee_buy_e6: u16::from_le_bytes(body.take(2)?.try_into().unwrap()),
            add_fee_sell_e6: u16::from_le_bytes(body.take(2)?.try_into().unwrap()),
        });
    }
    body.finish()?;
    Ok(MetricPool {
        kind,
        id,
        pool,
        pause_level,
        bin_index,
        position,
        distance_e6,
        spread_fee_e6,
        notional_fee_e8,
        asymmetry_x8,
        scale0,
        scale1,
        lowest_bin,
        highest_bin,
        token0,
        token1,
        reference_x64,
        provider,
        next_scan,
        bins,
        flags,
    })
}

fn encode_mutable(row: &MutableWrite, generation: u64) -> Vec<u8> {
    let mut out = vec![1];
    push_u64(&mut out, row.block);
    push_u64(&mut out, generation);
    out.extend_from_slice(row.bitmap.as_slice());
    push_u32(&mut out, row.tick_count);
    push_u8(&mut out, u8::from(row.truncated));
    match row.next_scan {
        Some(value) => {
            push_u8(&mut out, 1);
            push_i32(&mut out, value);
        }
        None => push_u8(&mut out, 0),
    }
    push_bytes(&mut out, &row.body);
    out
}

fn decode_mutable(bytes: &[u8]) -> eyre::Result<MutableView> {
    let view = decode_mutable_parts(bytes).map_err(|err| eyre::eyre!(err))?;
    Ok(view)
}

fn decode_mutable_parts(bytes: &[u8]) -> Result<MutableView, String> {
    let mut reader = Reader::new(bytes);
    if reader.u8()? != 1 {
        return Err("mutable".into());
    }
    let block = reader.u64()?;
    let generation = reader.u64()?;
    let bitmap = reader.hash()?;
    let tick_count = reader.u32()?;
    let truncated = reader.u8()? != 0;
    let next_scan = if reader.u8()? == 1 {
        Some(reader.i32()?)
    } else {
        None
    };
    let body = reader.bytes()?;
    reader.finish()?;
    Ok(MutableView {
        block,
        generation,
        bitmap,
        tick_count,
        truncated,
        next_scan,
        body,
    })
}

fn encode_dirty(block: u64, class: u8) -> Vec<u8> {
    let mut out = vec![1];
    push_u64(&mut out, block);
    push_u8(&mut out, class);
    out
}

fn decode_dirty(bytes: &[u8]) -> eyre::Result<(u64, u8)> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("dirty"));
    }
    let block = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let class = reader.u8().map_err(|err| eyre::eyre!(err))?;
    reader.finish().map_err(|err| eyre::eyre!(err))?;
    Ok((block, class))
}

fn encode_tick_page(page: &TickPage) -> eyre::Result<Vec<u8>> {
    let mut out = vec![1];
    push_u64(&mut out, page.generation);
    push_u32(&mut out, page.ticks.len() as u32);
    for tick in &page.ticks {
        push_i32(&mut out, tick.tick);
        push_u128(&mut out, tick.liquidity_gross);
        out.extend_from_slice(&tick.liquidity_net.to_le_bytes());
    }
    Ok(out)
}

fn tick_page_generation(bytes: &[u8]) -> eyre::Result<u64> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("tick page"));
    }
    reader.u64().map_err(|err| eyre::eyre!(err))
}

fn encode_curve(curve: &CurveRecord) -> Vec<u8> {
    let mut out = vec![1];
    push_u32(&mut out, curve.version);
    push_u64(&mut out, curve.generation);
    push_bytes(&mut out, &curve.payload);
    out
}

fn decode_curve_record(kind: u8, id: B256, bytes: &[u8]) -> eyre::Result<CurveRecord> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("curve"));
    }
    let version = reader.u32().map_err(|err| eyre::eyre!(err))?;
    let generation = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let payload = reader.bytes().map_err(|err| eyre::eyre!(err))?;
    reader.finish().map_err(|err| eyre::eyre!(err))?;
    Ok(CurveRecord {
        kind,
        id,
        generation,
        version,
        payload,
    })
}

fn encode_job(job: &WorkJob) -> Vec<u8> {
    let error = clip_error(&job.error);
    let mut out = vec![1];
    push_u64(&mut out, job.lease_until);
    push_u32(&mut out, job.attempt);
    push_u64(&mut out, job.not_before);
    push_u64(&mut out, job.generation);
    push_i32(&mut out, job.tick_cursor);
    push_u8(&mut out, job.class);
    let bytes = error.as_bytes();
    let len = u16::try_from(bytes.len()).unwrap_or(u16::MAX);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&bytes[..len as usize]);
    out
}

fn decode_job(key: &[u8], bytes: &[u8]) -> eyre::Result<WorkJob> {
    if key.len() != 34 {
        return Err(eyre::eyre!("work key"));
    }
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("work"));
    }
    let lease_until = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let attempt = reader.u32().map_err(|err| eyre::eyre!(err))?;
    let not_before = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let generation = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let tick_cursor = reader.i32().map_err(|err| eyre::eyre!(err))?;
    let class = reader.u8().map_err(|err| eyre::eyre!(err))?;
    let len = reader.u16().map_err(|err| eyre::eyre!(err))? as usize;
    let error = reader.take(len).map_err(|err| eyre::eyre!(err))?.to_vec();
    reader.finish().map_err(|err| eyre::eyre!(err))?;
    Ok(WorkJob {
        stage: key[0],
        kind: key[1],
        id: B256::from_slice(&key[2..]),
        lease_until,
        attempt,
        not_before,
        generation,
        tick_cursor,
        class,
        error: String::from_utf8_lossy(&error).into_owned(),
    })
}

fn word8(bytes: &[u8]) -> eyre::Result<[u8; 8]> {
    bytes
        .try_into()
        .map_err(|_| eyre::eyre!("mismatch counter"))
}

fn read_optional(
    read: &redb::ReadTransaction,
    table: TableDefinition<&[u8], &[u8]>,
    key: &[u8; 33],
) -> eyre::Result<Option<Vec<u8>>> {
    let table = match read.open_table(table) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    Ok(table
        .get(key.as_slice())?
        .map(|value| value.value().to_vec()))
}

fn read_tick_words(
    read: &redb::ReadTransaction,
    kind: u8,
    id: B256,
) -> eyre::Result<Vec<TickWord>> {
    let table = match read.open_table(TICK_PAGES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let start = tick_key(kind, id, 0);
    let end = tick_key(kind, id, u32::MAX);
    let mut ticks = Vec::new();
    for item in table.range::<&[u8]>(start.as_slice()..=end.as_slice())? {
        let (_key, value) = item?;
        ticks.extend(decode_tick_words(value.value())?);
    }
    ticks.sort_by_key(|tick| tick.tick);
    Ok(ticks)
}

fn decode_tick_words(bytes: &[u8]) -> eyre::Result<Vec<TickWord>> {
    let mut reader = Reader::new(bytes);
    if reader.u8().map_err(|err| eyre::eyre!(err))? != 1 {
        return Err(eyre::eyre!("tick page"));
    }
    let _generation = reader.u64().map_err(|err| eyre::eyre!(err))?;
    let n = reader.u32().map_err(|err| eyre::eyre!(err))? as usize;
    let mut ticks = Vec::with_capacity(n);
    for _ in 0..n {
        ticks.push(TickWord {
            tick: reader.i32().map_err(|err| eyre::eyre!(err))?,
            liquidity_gross: reader.u128().map_err(|err| eyre::eyre!(err))?,
            liquidity_net: reader.i128().map_err(|err| eyre::eyre!(err))?,
        });
    }
    Ok(ticks)
}

fn restamp_ticks(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    kind: u8,
    id: B256,
    generation: u64,
) -> eyre::Result<()> {
    let start = tick_key(kind, id, 0);
    let end = tick_key(kind, id, u32::MAX);
    let mut rows = Vec::new();
    for item in table.range::<&[u8]>(start.as_slice()..=end.as_slice())? {
        let (key, value) = item?;
        rows.push((key.value().to_vec(), value.value().to_vec()));
    }
    for (key, value) in rows {
        let mut ticks = decode_tick_words(&value)?;
        ticks.sort_by_key(|tick| tick.tick);
        let bytes = encode_tick_page(&TickPage {
            kind,
            id,
            page: u32::from_be_bytes(key[33..].try_into().unwrap()),
            generation,
            ticks,
        })?;
        table.insert(key.as_slice(), bytes.as_slice())?;
    }
    Ok(())
}

fn delete_prefix(table: &mut redb::Table<'_, &[u8], &[u8]>, prefix: &[u8; 33]) -> eyre::Result<()> {
    let mut start = [0u8; 37];
    start[..33].copy_from_slice(prefix);
    let mut end = start;
    end[33..].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut keys = Vec::new();
    for item in table.range::<&[u8]>(start.as_slice()..=end.as_slice())? {
        let (key, _) = item?;
        keys.push(key.value().to_vec());
    }
    for key in keys {
        table.remove(key.as_slice())?;
    }
    Ok(())
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
        assert_eq!(catalog.claim_work(STAGE_CURVE, 4, 1, 10).unwrap().len(), 0);
        assert!(catalog.reconcile(1).unwrap() >= 1);
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
        assert_eq!(curve.payload, vec![0, 5]);
    }
}
