//! Watch cursor and dirty flags.

use alloy_primitives::B256;
use redb::{Durability, ReadableDatabase, ReadableTable};
use std::collections::BTreeMap;
use store_model::{
    decode_dirty, decode_mutable, encode_dirty, entity_key, WatchHit, CLASS_HEAD, CLASS_NONE,
    CLASS_TICKS,
};

use super::super::catalog::Catalog;
use super::*;

impl Catalog {
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
