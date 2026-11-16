//! Kind mask and width sum stored beside each conduit.

use alloy_primitives::Address;
use redb::{Durability, ReadableDatabase, ReadableTable};
use routing::SlopeSegment;

use super::super::catalog::Catalog;
#[cfg(test)]
use super::codec::encode_conduit;
use super::codec::{decode_meta, encode_meta, meta_of, meta_of_row};
use super::*;

pub(super) fn write_meta(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    pair: &[u8; 40],
    segments: &[SlopeSegment],
) -> eyre::Result<()> {
    let bytes = encode_meta(&meta_of(segments));
    table.insert(pair.as_slice(), bytes.as_slice())?;
    Ok(())
}

pub(super) fn remove_meta(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    pair: &[u8; 40],
) -> eyre::Result<()> {
    table.remove(pair.as_slice())?;
    Ok(())
}

pub(crate) fn each_meta(
    read: &redb::ReadTransaction,
    mut visit: impl FnMut(Address, Address, ConduitMeta) -> eyre::Result<()>,
) -> eyre::Result<()> {
    let conduits = open_conduits(read)?;
    let metas = open_metas(read)?;
    let conduit_len = match &conduits {
        Some(table) => table.len()?,
        None => 0,
    };
    let meta_len = match &metas {
        Some(table) => table.len()?,
        None => 0,
    };
    if conduit_len == meta_len {
        if let Some(table) = metas {
            return visit_meta_table(&table, visit);
        }
        return Ok(());
    }
    let Some(table) = conduits else {
        return Ok(());
    };
    for item in table.iter()? {
        let (key, value) = item?;
        let Some((token_in, token_out)) = pair_of(key.value()) else {
            continue;
        };
        visit(token_in, token_out, meta_of_row(value.value())?)?;
    }
    Ok(())
}

impl Catalog {
    /// Write the kind mask and width sum for every conduit that does not have one.
    pub fn fill_conduit_meta(&self) -> eyre::Result<u64> {
        let read = self.entities.begin_read()?;
        let Some(conduits) = open_conduits(&read)? else {
            return Ok(0);
        };
        let metas = open_metas(&read)?;
        let conduit_len = conduits.len()?;
        let meta_len = match &metas {
            Some(table) => table.len()?,
            None => 0,
        };
        if conduit_len == meta_len {
            return Ok(0);
        }
        let mut missing = Vec::new();
        for item in conduits.iter()? {
            let (key, value) = item?;
            let raw = key.value();
            if raw.len() != 40 {
                continue;
            }
            if let Some(table) = &metas {
                if table.get(raw)?.is_some() {
                    continue;
                }
            }
            let bytes = encode_meta(&meta_of_row(value.value())?);
            let mut pair = [0_u8; 40];
            pair.copy_from_slice(raw);
            missing.push((pair, bytes));
        }
        drop(metas);
        drop(conduits);
        drop(read);
        let mut wrote = 0_u64;
        for chunk in missing.chunks(4096) {
            let mut write = self.entities.begin_write()?;
            write.set_durability(Durability::Immediate)?;
            {
                let conduits = write.open_table(CONDUITS)?;
                let mut table = write.open_table(META)?;
                for (pair, bytes) in chunk {
                    if conduits.get(pair.as_slice())?.is_none() {
                        continue;
                    }
                    if table.get(pair.as_slice())?.is_some() {
                        continue;
                    }
                    table.insert(pair.as_slice(), bytes.as_slice())?;
                    wrote += 1;
                }
            }
            write.commit()?;
        }
        Ok(wrote)
    }
}

#[cfg(test)]
impl Catalog {
    pub(crate) fn insert_conduit_without_meta_for_test(
        &self,
        token_in: Address,
        token_out: Address,
        segments: &[SlopeSegment],
    ) -> eyre::Result<()> {
        let mut write = self.entities.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut conduits = write.open_table(CONDUITS)?;
            let encoded = encode_conduit(1, 1, 0, segments)?;
            let pair = pair_key(token_in, token_out);
            conduits.insert(pair.as_slice(), encoded.as_slice())?;
        }
        write.commit()?;
        Ok(())
    }
}

fn open_metas(
    read: &redb::ReadTransaction,
) -> eyre::Result<Option<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    open_bytes(read, META)
}

fn visit_meta_table(
    table: &redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
    mut visit: impl FnMut(Address, Address, ConduitMeta) -> eyre::Result<()>,
) -> eyre::Result<()> {
    for item in table.iter()? {
        let (key, value) = item?;
        let Some((token_in, token_out)) = pair_of(key.value()) else {
            continue;
        };
        visit(token_in, token_out, decode_meta(value.value())?)?;
    }
    Ok(())
}

fn pair_of(raw: &[u8]) -> Option<(Address, Address)> {
    if raw.len() != 40 {
        return None;
    }
    let mut pair = [0_u8; 40];
    pair.copy_from_slice(raw);
    Some(pair_addresses(&pair))
}
