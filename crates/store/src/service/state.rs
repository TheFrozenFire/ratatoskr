//! Pool-state tables in `entities.redb` and the work queue in `work.redb`.

use super::catalog::{Catalog, VENUES};
use alloy_primitives::{B256, U256};
use redb::{Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};
use store_model::{
    decode_curve_record, decode_dirty, decode_job, decode_mutable, decode_quote_record,
    decode_tick_words, encode_curve, encode_dirty, encode_job, encode_mutable, encode_quote,
    encode_tick_page, entity_key, priority_of, tick_page_generation, CurveRecord, Dirty,
    ImmutablePut, ImmutableRow, MutableStored, MutableView, MutableWrite, PoolState, QuoteRecord,
    StateSummary, TickPage, TickWord, VenueRow, WatchHit, WorkFail, WorkJob, AERO, CLASS_HEAD,
    CLASS_NONE, CLASS_TICKS, CURVE_VERSION, METRIC, PANCAKE_V3, STAGE_CONDUIT, STAGE_CURVE,
    STAGE_IMMUTABLE, STAGE_MUTABLE, STAGE_QUOTE, STAGE_TICKS, SUSHI_V3, TICK_PAGE, UNI_V3, UNI_V4,
    V3_FORK,
};

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

const WATCH_KEY: &str = "watch_through";

impl Catalog {
    pub fn state_summary(&self) -> eyre::Result<StateSummary> {
        let mut summary = StateSummary {
            identities: count_bytes(&self.entities, VENUES)?,
            immutable: count_bytes(&self.entities, IMMUTABLE)?,
            mutable: count_bytes(&self.entities, MUTABLE)?,
            curves: count_bytes(&self.entities, CURVES)?,
            conduits: super::conduit::count_conduits(&self.entities)?,
            quotes: count_bytes(&self.entities, QUOTES)?,
            tick_pages: count_bytes(&self.entities, TICK_PAGES)?,
            mismatches: count_bytes(&self.entities, MISMATCH)?,
            tokens: self.token_count()?,
            ..StateSummary::default()
        };
        let read = self.work.begin_read()?;
        match read.open_table(JOBS) {
            Ok(table) => {
                for item in table.iter()? {
                    let (key, value) = item?;
                    let job = decode_job(key.value(), value.value())?;
                    summary.jobs += 1;
                    *summary.jobs_by_stage.entry(job.stage).or_default() += 1;
                    if !job.error.is_empty() {
                        *summary.errors.entry(job.error).or_default() += 1;
                    }
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
        drop(read);
        let (conduit_jobs, conduit_errors) = super::conduit::account_jobs(&self.work)?;
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
        super::catalog::read_venue_row(&self.entities, kind, id)
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
            super::conduit::retarget(self, row.kind, row.id, Some(&previous_curve), None)?;
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
        super::conduit::retarget(self, curve.kind, curve.id, previous.as_ref(), Some(curve))?;
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
            super::conduit::retarget(self, curve.kind, curve.id, previous.as_ref(), Some(curve))?;
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
                    if !super::hooks::has_trade_size(row.kind, row.id, state)? {
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
                super::conduit::retarget(self, curve.kind, curve.id, Some(curve), None)?;
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
            let top = super::hooks::capacity(*kind, *id, &state)?
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
            let top = super::hooks::capacity(row.kind, row.id, state)?
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
        let top = match super::hooks::capacity(row.kind, row.id, state)? {
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

fn count_bytes(db: &redb::Database, def: TableDefinition<&[u8], &[u8]>) -> eyre::Result<u64> {
    let read = db.begin_read()?;
    let table = match read.open_table(def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    Ok(table.len()?)
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

fn tick_key(kind: u8, id: B256, page: u32) -> [u8; 37] {
    let mut out = [0u8; 37];
    out[..33].copy_from_slice(&entity_key(kind, &id));
    out[33..].copy_from_slice(&page.to_be_bytes());
    out
}

fn clip_error(error: &str) -> String {
    error.chars().take(160).collect()
}

fn curve_is_current(curve: Option<&CurveRecord>, generation: u64) -> bool {
    match curve {
        Some(curve) => curve.generation == generation && curve.version == CURVE_VERSION,
        None => false,
    }
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

fn stride_pick(ids: &[(u8, B256)], remove: usize) -> Vec<(u8, B256)> {
    if remove == 0 || ids.is_empty() {
        return Vec::new();
    }
    if remove >= ids.len() {
        return ids.to_vec();
    }
    let mut out = Vec::with_capacity(remove);
    let mut acc = 0_usize;
    for id in ids {
        acc += remove;
        if acc >= ids.len() {
            acc -= ids.len();
            out.push(*id);
        }
    }
    out
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
