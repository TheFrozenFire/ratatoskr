//! Pair conduit: the slope merge of every direct action that spends one token
//! and pays another.
//!
//! At input `x` the value is the most those actions can pay together,
//! `F(x) = max { sum f_i(x_i) | x_i >= 0, sum x_i <= x }`. The stored row is
//! that merge. A path through a third token is a composition of these rows.
//! The actions in one row do not share a pool. Each row is built as though it
//! had the pool alone.

use super::catalog::Catalog;
use super::hooks;
use alloy_primitives::{Address, B256};
use num_bigint::{BigInt, Sign};
use redb::{Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};
use routing::{Action, SlopeSegment};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use store_model::{
    decode_conduit_job, decode_curve_record, encode_conduit_job, ConduitFail, ConduitJob,
    CurveRecord, CURVE_VERSION,
};

fn encode_job(job: &ConduitJob) -> Vec<u8> {
    encode_conduit_job(job)
}

fn decode_job(key: &[u8], bytes: &[u8]) -> eyre::Result<ConduitJob> {
    decode_conduit_job(key, bytes)
}

const CONDUITS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduits");
const MEMBERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_members");
const JOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_jobs");

const CURVES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("curves");

/// The stored slope merge for one ordered pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredConduit {
    pub block: u64,
    pub generation: u64,
    pub members: u32,
    pub segments: Vec<SlopeSegment>,
}

struct Member {
    key: [u8; 73],
    kind: u8,
    id: B256,
    generation: u64,
}

pub(crate) fn count_conduits(db: &redb::Database) -> eyre::Result<u64> {
    count_table(db, CONDUITS)
}

pub(crate) fn account_jobs(db: &redb::Database) -> eyre::Result<(u64, BTreeMap<String, u64>)> {
    let mut errors = BTreeMap::new();
    let read = db.begin_read()?;
    let table = match read.open_table(JOBS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok((0, errors)),
        Err(err) => return Err(err.into()),
    };
    let mut jobs = 0_u64;
    for item in table.iter()? {
        let (key, value) = item?;
        let job = decode_job(key.value(), value.value())?;
        jobs += 1;
        if !job.error.is_empty() {
            *errors.entry(job.error).or_default() += 1;
        }
    }
    Ok((jobs, errors))
}

/// Curve generations changed for this pool. Members move with the priced
/// directions, and each touched pair is queued. A pair with no members left
/// loses its conduit row.
pub(crate) fn retarget(
    catalog: &Catalog,
    kind: u8,
    id: B256,
    previous: Option<&CurveRecord>,
    next: Option<&CurveRecord>,
) -> eyre::Result<()> {
    let old_pairs = match previous {
        Some(curve) => hooks::priced_pairs(curve)?,
        None => Vec::new(),
    };
    let new_pairs = match next {
        Some(curve) => hooks::priced_pairs(curve)?,
        None => Vec::new(),
    };
    let generation = next.map(|curve| curve.generation);
    let mut pairs = BTreeSet::new();
    pairs.extend(old_pairs.iter().copied());
    pairs.extend(new_pairs.iter().copied());
    if pairs.is_empty() {
        return Ok(());
    }
    let now = super::state::unix_now();
    let mut alive = Vec::new();
    let mut gone = Vec::new();
    {
        let mut write = catalog.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut members = write.open_table(MEMBERS)?;
            let mut conduits = write.open_table(CONDUITS)?;
            for pair in &pairs {
                let key = member_key(&pair_key(pair.0, pair.1), kind, &id);
                if new_pairs.contains(pair) {
                    let Some(generation) = generation else {
                        continue;
                    };
                    let bytes = generation.to_le_bytes();
                    members.insert(key.as_slice(), bytes.as_slice())?;
                } else {
                    members.remove(key.as_slice())?;
                }
            }
            for pair in &pairs {
                let stored = pair_key(pair.0, pair.1);
                if write_members(&members, &stored)?.is_empty() {
                    conduits.remove(stored.as_slice())?;
                    gone.push(*pair);
                } else {
                    alive.push(*pair);
                }
            }
        }
        write.commit()?;
    }
    sync_jobs(catalog, &alive, &gone, now)
}

impl Catalog {
    /// Queue one job per ordered pair whose conduit is missing or behind the
    /// curves already stored. A pair whose member stamps and conduit row
    /// already match is left alone.
    pub fn enqueue_conduit_fill(&self) -> eyre::Result<u64> {
        let desired = desired_members(&self.entities)?;
        let now = super::state::unix_now();
        let mut dirty = BTreeSet::new();
        {
            let mut write = self.entities.begin_write()?;
            write.set_durability(Durability::Immediate)?;
            {
                let mut members = write.open_table(MEMBERS)?;
                let mut stale = Vec::new();
                let mut edited = Vec::new();
                for item in members.iter()? {
                    let (key, value) = item?;
                    let key_bytes = key.value().to_vec();
                    let Some((pair, kind, id)) = split_member(&key_bytes) else {
                        stale.push(key_bytes);
                        continue;
                    };
                    let owned = member_key(&pair, kind, &id);
                    match desired.get(&owned) {
                        Some(generation)
                            if value.value() == generation.to_le_bytes().as_slice() => {}
                        Some(generation) => {
                            edited.push((owned, *generation));
                            dirty.insert(pair);
                        }
                        None => {
                            stale.push(key_bytes);
                            dirty.insert(pair);
                        }
                    }
                }
                for key in stale {
                    members.remove(key.as_slice())?;
                }
                for (key, generation) in edited {
                    let bytes = generation.to_le_bytes();
                    members.insert(key.as_slice(), bytes.as_slice())?;
                }
                for (key, generation) in &desired {
                    if members.get(key.as_slice())?.is_none() {
                        let bytes = generation.to_le_bytes();
                        members.insert(key.as_slice(), bytes.as_slice())?;
                        if let Some((pair, _, _)) = split_member(key) {
                            dirty.insert(pair);
                        }
                    }
                }
            }
            write.commit()?;
        }
        let mut queued = 0_u64;
        let mut alive = Vec::new();
        let mut gone = Vec::new();
        let read = self.entities.begin_read()?;
        let members = open_members(&read)?;
        let conduits = open_conduits(&read)?;
        let mut seen = BTreeSet::new();
        for key in desired.keys() {
            let Some((pair, _, _)) = split_member(key) else {
                continue;
            };
            if !seen.insert(pair) {
                continue;
            }
            let rows = read_members(&members, &pair)?;
            let count = rows.len() as u32;
            let max_gen = rows
                .iter()
                .map(|member| member.generation)
                .max()
                .unwrap_or(0);
            let current = conduit_stamp(conduits.as_ref(), &pair)?;
            let matches = current.is_some_and(|(generation, stored_count)| {
                generation == max_gen && stored_count == count
            });
            if dirty.contains(&pair) || !matches {
                alive.push(pair_addresses(&pair));
                queued += 1;
            }
        }
        for pair in &dirty {
            if seen.contains(pair) {
                continue;
            }
            gone.push(pair_addresses(pair));
        }
        drop(members);
        drop(conduits);
        drop(read);
        if !gone.is_empty() {
            let mut write = self.entities.begin_write()?;
            write.set_durability(Durability::Immediate)?;
            {
                let mut conduits = write.open_table(CONDUITS)?;
                for pair in &gone {
                    conduits.remove(pair_key(pair.0, pair.1).as_slice())?;
                }
            }
            write.commit()?;
        }
        sync_jobs(self, &alive, &gone, now)?;
        Ok(queued)
    }

    pub fn claim_conduits(
        &self,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<ConduitJob>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let jobs = {
            let mut table = write.open_table(JOBS)?;
            let mut picked = Vec::new();
            for item in table.iter()? {
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
                job.rebuild = false;
                picked.push((key.value().to_vec(), job));
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

    /// `again` keeps the job after a successful build whose members moved.
    pub fn finish_conduits(
        &self,
        done: &[(Address, Address)],
        again: &[(Address, Address)],
        failed: &[ConduitFail],
    ) -> eyre::Result<()> {
        let again_set: BTreeSet<(Address, Address)> = again.iter().copied().collect();
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut table = write.open_table(JOBS)?;
            for pair in done {
                let key = pair_key(pair.0, pair.1);
                let Some(stored) = table.get(key.as_slice())? else {
                    continue;
                };
                let mut job = decode_job(key.as_slice(), stored.value())?;
                drop(stored);
                if job.rebuild || again_set.contains(pair) {
                    job.rebuild = false;
                    job.lease_until = 0;
                    job.not_before = 0;
                    job.error.clear();
                    let bytes = encode_job(&job);
                    table.insert(key.as_slice(), bytes.as_slice())?;
                } else {
                    table.remove(key.as_slice())?;
                }
            }
            for fail in failed {
                let key = pair_key(fail.token_in, fail.token_out);
                let Some(stored) = table.get(key.as_slice())? else {
                    continue;
                };
                let mut job = decode_job(key.as_slice(), stored.value())?;
                drop(stored);
                job.lease_until = 0;
                job.not_before = fail.not_before;
                job.error = clip_error(&fail.error);
                if job.rebuild {
                    job.rebuild = false;
                    job.not_before = 0;
                }
                let bytes = encode_job(&job);
                table.insert(key.as_slice(), bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// Build every conduit job that is ready. A job that fails stays for a later pass.
    pub fn drain_conduits(&self) -> eyre::Result<u64> {
        let mut built = 0_u64;
        loop {
            let now = super::state::unix_now();
            let jobs = self.claim_conduits(32, now, 120)?;
            if jobs.is_empty() {
                return Ok(built);
            }
            let mut done = Vec::new();
            let mut again = Vec::new();
            let mut failed = Vec::new();
            for job in &jobs {
                match self.build_conduit(job.token_in, job.token_out) {
                    Ok(true) => {
                        again.push((job.token_in, job.token_out));
                        done.push((job.token_in, job.token_out));
                    }
                    Ok(false) => done.push((job.token_in, job.token_out)),
                    Err(err) => failed.push(ConduitFail {
                        token_in: job.token_in,
                        token_out: job.token_out,
                        not_before: now.saturating_add(60),
                        error: clip_error(&err.to_string()),
                    }),
                }
            }
            self.finish_conduits(&done, &again, &failed)?;
            built += done.len() as u64;
            if built.is_multiple_of(256) {
                tracing::info!(built, "conduit drain");
            }
        }
    }

    /// Merge the members of one ordered pair and store the slope segments.
    ///
    /// The bool asks for another pass. It is set when the member index moved
    /// while this merge was reading.
    pub fn build_conduit(&self, token_in: Address, token_out: Address) -> eyre::Result<bool> {
        let pair = pair_key(token_in, token_out);
        let snapshot = {
            let read = self.entities.begin_read()?;
            let members = open_members(&read)?;
            read_members(&members, &pair)?
        };
        let mut kept = Vec::new();
        let mut parts = Vec::new();
        let mut min_block = u64::MAX;
        let mut max_gen = 0_u64;
        for member in &snapshot {
            let Some(built) = self.member_segments(token_in, token_out, member)? else {
                continue;
            };
            min_block = min_block.min(built.block);
            max_gen = max_gen.max(built.generation);
            parts.push(built.segments);
            kept.push(member.key);
        }
        let merged = routing::merge(&parts);
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let again = {
            let mut members = write.open_table(MEMBERS)?;
            let live = write_members(&members, &pair)?;
            if !same_members(&snapshot, &live) {
                true
            } else {
                for member in &snapshot {
                    if !kept.contains(&member.key) {
                        members.remove(member.key.as_slice())?;
                    }
                }
                let mut conduits = write.open_table(CONDUITS)?;
                if kept.is_empty() {
                    conduits.remove(pair.as_slice())?;
                } else {
                    let body = encode_conduit(min_block, max_gen, kept.len() as u32, &merged)?;
                    conduits.insert(pair.as_slice(), body.as_slice())?;
                }
                false
            }
        };
        write.commit()?;
        Ok(again)
    }

    pub fn read_conduit(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<StoredConduit>> {
        read_conduit_at(&self.entities, token_in, token_out)
    }

    /// Read every stored pair conduit. Empty curves are included.
    pub fn visit_conduits(
        &self,
        visit: impl FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        visit_conduits_at(&self.entities, visit)
    }

    /// Read every stored pair conduit, including the block the merge used.
    pub fn visit_conduit_blocks(
        &self,
        visit: impl FnMut(Address, Address, u64, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        visit_conduit_rows(&self.entities, visit)
    }

    fn member_segments(
        &self,
        token_in: Address,
        token_out: Address,
        member: &Member,
    ) -> eyre::Result<Option<Built>> {
        let state = self.read_state(member.kind, member.id)?;
        let (Some(immutable), Some(mutable), Some(curve)) =
            (state.immutable, state.mutable, state.curve)
        else {
            return Ok(None);
        };
        if curve.version != CURVE_VERSION
            || curve.generation != mutable.generation
            || curve.generation != member.generation
        {
            return Ok(None);
        }
        if !hooks::priced_pairs(&curve)?.contains(&(token_in, token_out)) {
            return Ok(None);
        }
        let Some(segments) = hooks::segments(&hooks::DirectionRows {
            immutable: &immutable,
            mutable_body: &mutable.body,
            ticks: &state.ticks,
            kind: member.kind,
            id: member.id,
            curve: &curve,
            token_in,
            token_out,
        })?
        else {
            return Ok(None);
        };
        Ok(Some(Built {
            block: mutable.block,
            generation: curve.generation,
            segments,
        }))
    }
}

pub(crate) fn read_conduit_at(
    db: &impl redb::ReadableDatabase,
    token_in: Address,
    token_out: Address,
) -> eyre::Result<Option<StoredConduit>> {
    let read = db.begin_read()?;
    let Some(table) = open_conduits(&read)? else {
        return Ok(None);
    };
    let key = pair_key(token_in, token_out);
    match table.get(key.as_slice())? {
        Some(value) => Ok(Some(decode_conduit(value.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn visit_conduits_at(
    db: &impl redb::ReadableDatabase,
    mut visit: impl FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
) -> eyre::Result<()> {
    visit_conduit_rows(db, |token_in, token_out, _block, segments| {
        visit(token_in, token_out, segments)
    })
}

pub(crate) fn visit_conduit_rows(
    db: &impl redb::ReadableDatabase,
    mut visit: impl FnMut(Address, Address, u64, &[SlopeSegment]) -> eyre::Result<()>,
) -> eyre::Result<()> {
    let read = db.begin_read()?;
    let Some(table) = open_conduits(&read)? else {
        return Ok(());
    };
    for item in table.iter()? {
        let (key, value) = item?;
        let raw = key.value();
        if raw.len() != 40 {
            continue;
        }
        let mut pair = [0_u8; 40];
        pair.copy_from_slice(raw);
        let (token_in, token_out) = pair_addresses(&pair);
        let stored = decode_conduit(value.value())?;
        visit(token_in, token_out, stored.block, &stored.segments)?;
    }
    Ok(())
}

impl super::catalog::EntityRead {
    pub fn read_conduit(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<StoredConduit>> {
        read_conduit_at(&self.entities, token_in, token_out)
    }

    /// Read every stored pair conduit. Empty curves are included.
    pub fn visit_conduits(
        &self,
        visit: impl FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        visit_conduits_at(&self.entities, visit)
    }

    /// Read every stored pair conduit, including the block the merge used.
    pub fn visit_conduit_blocks(
        &self,
        visit: impl FnMut(Address, Address, u64, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        visit_conduit_rows(&self.entities, visit)
    }
}

struct Built {
    block: u64,
    generation: u64,
    segments: Vec<SlopeSegment>,
}

fn desired_members(db: &redb::Database) -> eyre::Result<HashMap<[u8; 73], u64>> {
    let mut desired = HashMap::new();
    let mut curves = 0_u64;
    let read = db.begin_read()?;
    let table = match read.open_table(CURVES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(desired),
        Err(err) => return Err(err.into()),
    };
    for item in table.iter()? {
        let (key, value) = item?;
        let key = key.value();
        if key.len() != 33 {
            continue;
        }
        let kind = key[0];
        let id = B256::from_slice(&key[1..]);
        curves += 1;
        if curves.is_multiple_of(100_000) {
            tracing::info!(curves, "conduit fill scan");
        }
        let Ok(curve) = decode_curve_record(kind, id, value.value()) else {
            tracing::warn!(kind, %id, "conduit fill skipped a curve row");
            continue;
        };
        for (token_in, token_out) in hooks::priced_pairs(&curve)? {
            let member = member_key(&pair_key(token_in, token_out), kind, &id);
            desired.insert(member, curve.generation);
        }
    }
    tracing::info!(curves, members = desired.len(), "conduit fill scanned");
    Ok(desired)
}

fn sync_jobs(
    catalog: &Catalog,
    alive: &[(Address, Address)],
    gone: &[(Address, Address)],
    now: u64,
) -> eyre::Result<()> {
    if alive.is_empty() && gone.is_empty() {
        return Ok(());
    }
    let mut write = catalog.work.begin_write()?;
    write.set_durability(Durability::Immediate)?;
    {
        let mut table = write.open_table(JOBS)?;
        for pair in gone {
            let key = pair_key(pair.0, pair.1);
            let Some(stored) = table.get(key.as_slice())? else {
                continue;
            };
            let job = decode_job(key.as_slice(), stored.value())?;
            drop(stored);
            if job.lease_until > now {
                let mut job = job;
                job.rebuild = true;
                let bytes = encode_job(&job);
                table.insert(key.as_slice(), bytes.as_slice())?;
            } else {
                table.remove(key.as_slice())?;
            }
        }
        for pair in alive {
            let key = pair_key(pair.0, pair.1);
            let job = {
                let existing = table.get(key.as_slice())?;
                if let Some(stored) = existing {
                    let mut job = decode_job(key.as_slice(), stored.value())?;
                    drop(stored);
                    if job.lease_until > now {
                        job.rebuild = true;
                    } else {
                        job.not_before = 0;
                        job.error.clear();
                    }
                    job
                } else {
                    ConduitJob {
                        token_in: pair.0,
                        token_out: pair.1,
                        lease_until: 0,
                        attempt: 0,
                        not_before: 0,
                        rebuild: false,
                        error: String::new(),
                    }
                }
            };
            let bytes = encode_job(&job);
            table.insert(key.as_slice(), bytes.as_slice())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn same_members(left: &[Member], right: &[Member]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .all(|(left, right)| left.key == right.key && left.generation == right.generation)
}

fn conduit_stamp(
    table: Option<&redb::ReadOnlyTable<&'static [u8], &'static [u8]>>,
    pair: &[u8; 40],
) -> eyre::Result<Option<(u64, u32)>> {
    let Some(table) = table else {
        return Ok(None);
    };
    let Some(value) = table.get(pair.as_slice())? else {
        return Ok(None);
    };
    let stored = decode_conduit(value.value())?;
    Ok(Some((stored.generation, stored.members)))
}

macro_rules! collect_members {
    ($table:expr, $pair:expr) => {{
        let mut found = Vec::new();
        let start: &[u8] = $pair.as_slice();
        let end = prefix_end($pair);
        let push = |found: &mut Vec<Member>, key: &[u8], value: &[u8]| {
            let Some((pair, kind, id)) = split_member(key) else {
                return;
            };
            if pair != *$pair || value.len() != 8 {
                return;
            }
            let mut word = [0_u8; 8];
            word.copy_from_slice(value);
            found.push(Member {
                key: member_key(&pair, kind, &id),
                kind,
                id,
                generation: u64::from_le_bytes(word),
            });
        };
        if let Some(end) = end {
            for item in $table.range::<&[u8]>(start..end.as_slice())? {
                let (key, value) = item?;
                push(&mut found, key.value(), value.value());
            }
        } else {
            for item in $table.range::<&[u8]>(start..)? {
                let (key, value) = item?;
                push(&mut found, key.value(), value.value());
            }
        }
        found
    }};
}

fn read_members(
    table: &redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
    pair: &[u8; 40],
) -> eyre::Result<Vec<Member>> {
    Ok(collect_members!(table, pair))
}

fn write_members(
    table: &redb::Table<'_, &[u8], &[u8]>,
    pair: &[u8; 40],
) -> eyre::Result<Vec<Member>> {
    Ok(collect_members!(table, pair))
}

fn open_members(
    read: &redb::ReadTransaction,
) -> eyre::Result<redb::ReadOnlyTable<&'static [u8], &'static [u8]>> {
    match open_bytes(read, MEMBERS)? {
        Some(table) => Ok(table),
        None => Err(eyre::eyre!("conduit members")),
    }
}

fn open_conduits(
    read: &redb::ReadTransaction,
) -> eyre::Result<Option<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    open_bytes(read, CONDUITS)
}

fn open_bytes(
    read: &redb::ReadTransaction,
    def: TableDefinition<&'static [u8], &'static [u8]>,
) -> eyre::Result<Option<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    match read.open_table(def) {
        Ok(table) => Ok(Some(table)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Row count stored on the tree header. Walking the table reads every leaf.
fn count_table(db: &redb::Database, def: TableDefinition<&[u8], &[u8]>) -> eyre::Result<u64> {
    let read = db.begin_read()?;
    let table = match read.open_table(def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    Ok(table.len()?)
}

fn pair_key(token_in: Address, token_out: Address) -> [u8; 40] {
    let mut out = [0_u8; 40];
    out[..20].copy_from_slice(token_in.as_slice());
    out[20..].copy_from_slice(token_out.as_slice());
    out
}

fn pair_addresses(pair: &[u8; 40]) -> (Address, Address) {
    (
        Address::from_slice(&pair[..20]),
        Address::from_slice(&pair[20..]),
    )
}

fn member_key(pair: &[u8; 40], kind: u8, id: &B256) -> [u8; 73] {
    let mut out = [0_u8; 73];
    out[..40].copy_from_slice(pair);
    out[40] = kind;
    out[41..].copy_from_slice(id.as_slice());
    out
}

fn split_member(key: &[u8]) -> Option<([u8; 40], u8, B256)> {
    if key.len() != 73 {
        return None;
    }
    let mut pair = [0_u8; 40];
    pair.copy_from_slice(&key[..40]);
    Some((pair, key[40], B256::from_slice(&key[41..])))
}

fn prefix_end(prefix: &[u8; 40]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last != u8::MAX {
            *last += 1;
            return Some(end);
        }
        end.pop();
    }
    None
}

fn encode_conduit(
    block: u64,
    generation: u64,
    members: u32,
    segments: &[SlopeSegment],
) -> eyre::Result<Vec<u8>> {
    let mut out = Vec::new();
    out.push(1);
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&generation.to_le_bytes());
    out.extend_from_slice(&members.to_le_bytes());
    let count = u32::try_from(segments.len()).map_err(|_| eyre::eyre!("conduit segments"))?;
    out.extend_from_slice(&count.to_le_bytes());
    for segment in segments {
        push_be32(&mut out, &segment.width)?;
        push_be32(&mut out, &segment.output)?;
        out.push(segment.action.kind);
        out.extend_from_slice(&segment.action.pool);
    }
    Ok(out)
}

fn decode_conduit(bytes: &[u8]) -> eyre::Result<StoredConduit> {
    let mut reader = Reader::new(bytes);
    if reader.u8()? != 1 {
        return Err(eyre::eyre!("conduit"));
    }
    let block = reader.u64()?;
    let generation = reader.u64()?;
    let members = reader.u32()?;
    let count = reader.u32()? as usize;
    let mut segments = Vec::with_capacity(count);
    for _ in 0..count {
        let width = reader.be32()?;
        let output = reader.be32()?;
        let kind = reader.u8()?;
        let mut pool = [0_u8; 32];
        pool.copy_from_slice(reader.take(32)?);
        segments.push(SlopeSegment {
            width,
            output,
            action: Action { kind, pool },
        });
    }
    reader.finish()?;
    Ok(StoredConduit {
        block,
        generation,
        members,
        segments,
    })
}

fn push_be32(out: &mut Vec<u8>, value: &BigInt) -> eyre::Result<()> {
    let (sign, bytes) = value.to_bytes_be();
    if sign == Sign::Minus || bytes.len() > 32 {
        return Err(eyre::eyre!("segment magnitude"));
    }
    let mut word = [0_u8; 32];
    word[32 - bytes.len()..].copy_from_slice(&bytes);
    out.extend_from_slice(&word);
    Ok(())
}

fn clip_error(error: &str) -> String {
    let mut end = error.len().min(180);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> eyre::Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| eyre::eyre!("conduit short"))?;
        if end > self.bytes.len() {
            return Err(eyre::eyre!("conduit short"));
        }
        let start = self.at;
        self.at = end;
        Ok(&self.bytes[start..end])
    }

    fn u8(&mut self) -> eyre::Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> eyre::Result<u32> {
        let mut word = [0_u8; 4];
        word.copy_from_slice(self.take(4)?);
        Ok(u32::from_le_bytes(word))
    }

    fn u64(&mut self) -> eyre::Result<u64> {
        let mut word = [0_u8; 8];
        word.copy_from_slice(self.take(8)?);
        Ok(u64::from_le_bytes(word))
    }

    fn be32(&mut self) -> eyre::Result<BigInt> {
        Ok(BigInt::from_bytes_be(Sign::Plus, self.take(32)?))
    }

    fn finish(self) -> eyre::Result<()> {
        if self.at != self.bytes.len() {
            return Err(eyre::eyre!("conduit tail"));
        }
        Ok(())
    }
}
