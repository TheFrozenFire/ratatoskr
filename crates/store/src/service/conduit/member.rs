//! Member index and the stored slope merge.

use alloy_primitives::{Address, B256};
use redb::{Durability, ReadableDatabase, ReadableTable};
use std::collections::{BTreeSet, HashMap};
use std::ops::Bound;
use store_model::{decode_curve_record, entity_key};

use super::super::catalog::Catalog;
use super::codec::{decode_conduit, encode_conduit};
use super::job::sync_jobs;
use super::meta::{remove_meta, write_meta};
use super::*;
use super::{ConduitBody, ConduitMember, CurvePage, CurveRow, MemberTarget};

impl Catalog {
    /// Move one pool's member rows to `new_pairs` and queue every pair that changed.
    pub fn apply_retarget(
        &self,
        kind: u8,
        id: B256,
        old_pairs: &[(Address, Address)],
        new_pairs: &[(Address, Address)],
        generation: Option<u64>,
    ) -> eyre::Result<()> {
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
            let mut write = self.entities.begin_write()?;
            write.set_durability(Durability::Immediate)?;
            {
                let mut members = write.open_table(MEMBERS)?;
                let mut conduits = write.open_table(CONDUITS)?;
                let mut metas = write.open_table(META)?;
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
                        remove_meta(&mut metas, &stored)?;
                        gone.push(*pair);
                    } else {
                        alive.push(*pair);
                    }
                }
            }
            write.commit()?;
        }
        sync_jobs(self, &alive, &gone, now)
    }
}

impl Catalog {
    /// Visit at most `limit` curve keys after `after`, in key order.
    ///
    /// A key that does not decode is still a visited row. The cursor is that key.
    pub fn page_curves(&self, after: Option<(u8, B256)>, limit: usize) -> eyre::Result<CurvePage> {
        if limit == 0 {
            return Ok(CurvePage {
                rows: Vec::new(),
                resume: after,
                more: false,
            });
        }
        let read = self.entities.begin_read()?;
        let table = match read.open_table(CURVES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(CurvePage {
                    rows: Vec::new(),
                    resume: None,
                    more: false,
                });
            }
            Err(err) => return Err(err.into()),
        };
        let start_key = after.map(|(kind, id)| entity_key(kind, &id));
        let mut rows = Vec::new();
        let mut resume = None;
        let mut more = false;
        let mut visit = |key: &[u8], value: &[u8]| -> bool {
            if key.len() != 33 {
                return false;
            }
            if rows.len() == limit {
                more = true;
                return true;
            }
            let kind = key[0];
            let id = B256::from_slice(&key[1..]);
            resume = Some((kind, id));
            let curve = decode_curve_record(kind, id, value).ok();
            rows.push(CurveRow { kind, id, curve });
            false
        };
        if let Some(start_key) = &start_key {
            for item in
                table.range::<&[u8]>((Bound::Excluded(start_key.as_slice()), Bound::Unbounded))?
            {
                let (key, value) = item?;
                if visit(key.value(), value.value()) {
                    break;
                }
            }
        } else {
            for item in table.iter()? {
                let (key, value) = item?;
                if visit(key.value(), value.value()) {
                    break;
                }
            }
        }
        Ok(CurvePage { rows, resume, more })
    }

    /// Make the member index match `targets` and queue pairs whose conduit is behind.
    pub fn enqueue_members(&self, targets: &[MemberTarget]) -> eyre::Result<u64> {
        let mut desired = HashMap::new();
        for target in targets {
            let key = member_key(
                &pair_key(target.token_in, target.token_out),
                target.kind,
                &target.id,
            );
            desired.insert(key, target.generation);
        }
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
                let mut metas = write.open_table(META)?;
                for pair in &gone {
                    let key = pair_key(pair.0, pair.1);
                    conduits.remove(key.as_slice())?;
                    remove_meta(&mut metas, &key)?;
                }
            }
            write.commit()?;
        }
        sync_jobs(self, &alive, &gone, now)?;
        Ok(queued)
    }

    /// Members of one ordered pair, in key order.
    pub fn conduit_members(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Vec<ConduitMember>> {
        let pair = pair_key(token_in, token_out);
        let read = self.entities.begin_read()?;
        let members = open_members(&read)?;
        read_members(&members, &pair)
    }

    /// Store `body` when the member index still matches the snapshot it was built from.
    ///
    /// The bool asks for another pass. It is set when the member index moved
    /// while this merge was reading.
    pub fn store_conduit(&self, body: &ConduitBody) -> eyre::Result<bool> {
        let pair = pair_key(body.token_in, body.token_out);
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let again = {
            let mut members = write.open_table(MEMBERS)?;
            let live = write_members(&members, &pair)?;
            if !same_members(&body.snapshot, &live) {
                true
            } else {
                for member in &body.snapshot {
                    if !body.kept.contains(&member.key) {
                        members.remove(member.key.as_slice())?;
                    }
                }
                let mut conduits = write.open_table(CONDUITS)?;
                let mut metas = write.open_table(META)?;
                if body.kept.is_empty() {
                    conduits.remove(pair.as_slice())?;
                    remove_meta(&mut metas, &pair)?;
                } else {
                    let encoded = encode_conduit(
                        body.block,
                        body.generation,
                        body.kept.len() as u32,
                        &body.segments,
                    )?;
                    conduits.insert(pair.as_slice(), encoded.as_slice())?;
                    write_meta(&mut metas, &pair, &body.segments)?;
                }
                false
            }
        };
        write.commit()?;
        Ok(again)
    }
}

fn same_members(left: &[ConduitMember], right: &[ConduitMember]) -> bool {
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
        let push = |found: &mut Vec<ConduitMember>, key: &[u8], value: &[u8]| {
            let Some((pair, kind, id)) = split_member(key) else {
                return;
            };
            if pair != *$pair || value.len() != 8 {
                return;
            }
            let mut word = [0_u8; 8];
            word.copy_from_slice(value);
            found.push(ConduitMember {
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
) -> eyre::Result<Vec<ConduitMember>> {
    Ok(collect_members!(table, pair))
}

fn write_members(
    table: &redb::Table<'_, &[u8], &[u8]>,
    pair: &[u8; 40],
) -> eyre::Result<Vec<ConduitMember>> {
    Ok(collect_members!(table, pair))
}
