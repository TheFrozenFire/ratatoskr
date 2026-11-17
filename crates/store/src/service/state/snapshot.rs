//! Immutable rows, mutable snapshots, and tick pages.

use alloy_primitives::B256;
use redb::{Durability, ReadableDatabase, ReadableTable};
use store_model::{
    decode_curve_record, decode_dirty, decode_mutable, decode_tick_words, encode_mutable,
    encode_tick_page, entity_key, Dirty, ImmutablePut, ImmutableRow, MutableStored, MutableWrite,
    PoolState, TickPage, TickWord, CLASS_HEAD,
};

use super::super::catalog::{Catalog, EntityRead};
use super::*;

impl Catalog {
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
    /// The dirty flag is removed only when it does not name a newer block.
    pub fn put_mutable(&self, row: &MutableWrite) -> eyre::Result<MutableStored> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
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
            MutableStored {
                generation,
                dirty_cleared,
                kept_ticks,
                bitmap_changed,
                previous_curve,
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

/// One pool as the entity file stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolCard {
    pub immutable: bool,
    pub block: Option<u64>,
    pub generation: Option<u64>,
    pub tick_count: Option<u32>,
    pub truncated: Option<bool>,
    pub next_scan: Option<i32>,
    pub curve_generation: Option<u64>,
    pub curve_version: Option<u32>,
}

impl EntityRead {
    pub fn pool_card(&self, kind: u8, id: B256) -> eyre::Result<PoolCard> {
        let read = self.entities.begin_read()?;
        let key = entity_key(kind, &id);
        let immutable = read_optional(&read, IMMUTABLE, &key)?.is_some();
        let mutable = match read_optional(&read, MUTABLE, &key)? {
            Some(bytes) => Some(decode_mutable(&bytes)?),
            None => None,
        };
        let curve = match read_optional(&read, CURVES, &key)? {
            Some(bytes) => decode_curve_record(kind, id, &bytes).ok(),
            None => None,
        };
        Ok(PoolCard {
            immutable,
            block: mutable.as_ref().map(|row| row.block),
            generation: mutable.as_ref().map(|row| row.generation),
            tick_count: mutable.as_ref().map(|row| row.tick_count),
            truncated: mutable.as_ref().map(|row| row.truncated),
            next_scan: mutable.as_ref().and_then(|row| row.next_scan),
            curve_generation: curve.as_ref().map(|row| row.generation),
            curve_version: curve.as_ref().map(|row| row.version),
        })
    }
}
