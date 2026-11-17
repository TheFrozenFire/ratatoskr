//! Pool identity rows.

use alloy_primitives::{Address, B256};
use redb::{Database, Durability, ReadableDatabase, ReadableTable};
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use store_model::{decode_row, encode_row, entity_key, factory_of, tally_kind, Put, VenueRow};

use super::{Catalog, EntityRead, VENUES};

impl Catalog {
    /// At most `limit` rows of `kind` whose id is greater than `after`.
    ///
    /// The read stops when the page is full. Later keys are not visited.
    pub fn page_venues(
        &self,
        kind: u8,
        after: Option<B256>,
        limit: usize,
    ) -> eyre::Result<Vec<VenueRow>> {
        page_venue_rows(&self.entities, kind, after, limit)
    }

    pub fn tally_by_kind(&self) -> eyre::Result<BTreeMap<(u8, Address), u64>> {
        let read = self.entities.begin_read()?;
        let table = match read.open_table(VENUES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
            Err(err) => return Err(err.into()),
        };
        let mut counts = BTreeMap::new();
        for row in table.iter()? {
            let (key, value) = row?;
            let Some(kind) = tally_kind(key.value()) else {
                continue;
            };
            let factory = factory_of(value.value())?;
            *counts.entry((kind, factory)).or_default() += 1;
        }
        Ok(counts)
    }
}

impl EntityRead {
    pub fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        read_venue_row(&self.entities, kind, id)
    }

    /// At most `limit` rows of `kind` whose id is greater than `after`.
    pub fn page_venues(
        &self,
        kind: u8,
        after: Option<B256>,
        limit: usize,
    ) -> eyre::Result<Vec<VenueRow>> {
        page_venue_rows(&self.entities, kind, after, limit)
    }
}

fn page_venue_rows(
    db: &impl crate::storage::Handle,
    kind: u8,
    after: Option<B256>,
    limit: usize,
) -> eyre::Result<Vec<VenueRow>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let read = db.begin_read()?;
    let table = match read.open_table(VENUES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let start_key = match after {
        Some(id) => entity_key(kind, &id),
        None => entity_key(kind, &B256::ZERO),
    };
    let mut end_key = [0u8; 33];
    if kind != u8::MAX {
        end_key[0] = kind + 1;
    }
    let start = match after {
        Some(_) => Bound::Excluded(start_key.as_slice()),
        None => Bound::Included(start_key.as_slice()),
    };
    let end = if kind == u8::MAX {
        Bound::Unbounded
    } else {
        Bound::Excluded(end_key.as_slice())
    };
    let mut page = Vec::with_capacity(limit);
    for item in table.range::<&[u8]>((start, end))? {
        if page.len() == limit {
            break;
        }
        let (key_bytes, value) = item?;
        let key_bytes = key_bytes.value();
        if key_bytes.len() != 33 || key_bytes[0] != kind {
            return Err(eyre::eyre!("venue key"));
        }
        let id = B256::from_slice(&key_bytes[1..]);
        page.push(decode_row(kind, id, value.value())?);
    }
    Ok(page)
}

pub(super) fn write_identities(db: &Database, rows: &[VenueRow], put: Put) -> eyre::Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut write = db.begin_write()?;
    write.set_durability(Durability::Immediate)?;
    let written = {
        let mut venues = write.open_table(VENUES)?;
        let mut n = 0_u64;
        for row in rows {
            let key = row.key();
            let previous = venues
                .get(key.as_slice())?
                .map(|value| value.value().to_vec());
            let stored = match (previous, put) {
                (Some(_), Put::IfAbsent) => None,
                (Some(bytes), Put::Merge) => {
                    let old = decode_row(row.kind, row.id, &bytes)?;
                    Some(encode_row(&merge(old, row.clone()))?)
                }
                (None, _) => Some(encode_row(row)?),
            };
            if let Some(bytes) = stored {
                venues.insert(key.as_slice(), bytes.as_slice())?;
                n += 1;
            }
        }
        n
    };
    write.commit()?;
    Ok(written)
}

pub(super) fn merge(mut old: VenueRow, new: VenueRow) -> VenueRow {
    if new.factory != Address::ZERO {
        old.factory = new.factory;
    }
    if !new.tokens.is_empty() {
        old.tokens = new.tokens;
    }
    if new.fee.is_some() {
        old.fee = new.fee;
    }
    if new.tick_spacing.is_some() {
        old.tick_spacing = new.tick_spacing;
    }
    if new.block_number.is_some() {
        old.block_number = new.block_number;
    }
    if new.extra.is_some() {
        old.extra = new.extra;
    }
    if new.block_hash.is_some() && new.tx_index.is_some() && new.log_index.is_some() {
        old.block_hash = new.block_hash;
        old.tx_index = new.tx_index;
        old.log_index = new.log_index;
    }
    old
}

pub(crate) fn read_venue_row(
    db: &impl crate::storage::Handle,
    kind: u8,
    id: B256,
) -> eyre::Result<Option<VenueRow>> {
    let read = db.begin_read()?;
    venue_row(&read, kind, id)
}

pub(crate) fn venue_row(
    read: &redb::ReadTransaction,
    kind: u8,
    id: B256,
) -> eyre::Result<Option<VenueRow>> {
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

pub(super) fn identity_keys(catalog: &Catalog) -> eyre::Result<HashSet<[u8; 33]>> {
    let read = catalog.entities.begin_read()?;
    let table = match read.open_table(VENUES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashSet::new()),
        Err(err) => return Err(err.into()),
    };
    let mut keys = HashSet::new();
    for item in table.iter()? {
        let (key, _) = item?;
        let key = key.value();
        if key.len() != 33 {
            return Err(eyre::eyre!("venue key"));
        }
        let mut owned = [0u8; 33];
        owned.copy_from_slice(key);
        keys.insert(owned);
    }
    Ok(keys)
}

pub(super) fn count_identities(db: &Database) -> eyre::Result<BTreeMap<u8, u64>> {
    let read = db.begin_read()?;
    let table = match read.open_table(VENUES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(err) => return Err(err.into()),
    };
    let mut counts = BTreeMap::new();
    for (seen, item) in table.iter()?.enumerate() {
        if seen.is_multiple_of(100_000) {
            observability::info!(seen, "counting identities");
        }
        let (key, _) = item?;
        let key = key.value();
        if key.len() != 33 {
            return Err(eyre::eyre!("venue key"));
        }
        *counts.entry(key[0]).or_default() += 1;
    }
    observability::info!(rows = counts.values().sum::<u64>(), "counted identities");
    Ok(counts)
}
