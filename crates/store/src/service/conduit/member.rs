//! Member index and the slope merge.

use alloy_primitives::{Address, B256};
use redb::{Durability, ReadableDatabase, ReadableTable};
use routing::SlopeSegment;
use std::collections::{BTreeSet, HashMap};
use store_model::{decode_curve_record, CurveRecord, CURVE_VERSION};

use super::super::catalog::Catalog;
use super::super::hooks;
use super::codec::{decode_conduit, encode_conduit};
use super::job::sync_jobs;
use super::*;

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
    let now = super::super::state::unix_now();
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
        let now = super::super::state::unix_now();
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
            observability::info!(curves, "conduit fill scan");
        }
        let Ok(curve) = decode_curve_record(kind, id, value.value()) else {
            observability::warn!(kind, %id, "conduit fill skipped a curve row");
            continue;
        };
        for (token_in, token_out) in hooks::priced_pairs(&curve)? {
            let member = member_key(&pair_key(token_in, token_out), kind, &id);
            desired.insert(member, curve.generation);
        }
    }
    observability::info!(curves, members = desired.len(), "conduit fill scanned");
    Ok(desired)
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
