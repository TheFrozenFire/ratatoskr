//! Stored pair conduits.

use alloy_primitives::Address;
use redb::{ReadableDatabase, ReadableTable};
use routing::SlopeSegment;

use super::super::catalog::{Catalog, EntityRead};
use super::codec::{decode_conduit, meta_of};
use super::*;

/// One stored pair and the mask stored beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConduitRow {
    pub token_in: Address,
    pub token_out: Address,
    pub meta: ConduitMeta,
}

impl EntityRead {
    pub fn page_conduits(
        &self,
        after: Option<(Address, Address)>,
        limit: usize,
    ) -> eyre::Result<Vec<ConduitRow>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let read = self.entities.begin_read()?;
        let Some(table) = open_conduit_table(&read)? else {
            return Ok(Vec::new());
        };
        let mut page = Vec::new();
        for item in table.iter()? {
            if page.len() == limit {
                break;
            }
            let (key, value) = item?;
            let raw = key.value();
            if raw.len() != 40 {
                continue;
            }
            let token_in = Address::from_slice(&raw[..20]);
            let token_out = Address::from_slice(&raw[20..]);
            if let Some((after_in, after_out)) = after {
                if (token_in, token_out) <= (after_in, after_out) {
                    continue;
                }
            }
            let stored = decode_conduit(value.value())?;
            page.push(ConduitRow {
                token_in,
                token_out,
                meta: meta_of(&stored.segments),
            });
        }
        Ok(page)
    }

    pub fn conduit(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<StoredConduit>> {
        read_conduit_at(&self.entities, token_in, token_out)
    }

    /// Stored pairs that name `token` on either side.
    pub fn conduits_naming(&self, token: Address) -> eyre::Result<u64> {
        let read = self.entities.begin_read()?;
        let Some(table) = open_conduit_table(&read)? else {
            return Ok(0);
        };
        let raw = token.as_slice();
        let mut count = 0_u64;
        for item in table.iter()? {
            let (key, _) = item?;
            let bytes = key.value();
            if bytes.len() == 40 && (&bytes[..20] == raw || &bytes[20..] == raw) {
                count += 1;
            }
        }
        Ok(count)
    }
}

impl Catalog {
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

pub(crate) fn read_conduit_at(
    db: &impl crate::storage::Handle,
    token_in: Address,
    token_out: Address,
) -> eyre::Result<Option<StoredConduit>> {
    let read = db.begin_read()?;
    conduit_point(&read, token_in, token_out)
}

pub(crate) fn conduit_point(
    read: &redb::ReadTransaction,
    token_in: Address,
    token_out: Address,
) -> eyre::Result<Option<StoredConduit>> {
    let Some(table) = open_conduit_table(read)? else {
        return Ok(None);
    };
    conduit_get(&table, token_in, token_out)
}

pub(crate) fn open_conduit_table(
    read: &redb::ReadTransaction,
) -> eyre::Result<Option<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    open_conduits(read)
}

pub(crate) fn conduit_get(
    table: &redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
    token_in: Address,
    token_out: Address,
) -> eyre::Result<Option<StoredConduit>> {
    let key = pair_key(token_in, token_out);
    match table.get(key.as_slice())? {
        Some(value) => Ok(Some(decode_conduit(value.value())?)),
        None => Ok(None),
    }
}

pub(crate) fn visit_conduits_at(
    db: &impl crate::storage::Handle,
    mut visit: impl FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
) -> eyre::Result<()> {
    visit_conduit_rows(db, |token_in, token_out, _block, segments| {
        visit(token_in, token_out, segments)
    })
}

pub(crate) fn visit_conduit_rows(
    db: &impl crate::storage::Handle,
    visit: impl FnMut(Address, Address, u64, &[SlopeSegment]) -> eyre::Result<()>,
) -> eyre::Result<()> {
    let read = db.begin_read()?;
    conduit_rows(&read, visit)
}

pub(crate) fn conduit_rows(
    read: &redb::ReadTransaction,
    visit: impl FnMut(Address, Address, u64, &[SlopeSegment]) -> eyre::Result<()>,
) -> eyre::Result<()> {
    let Some(table) = open_conduit_table(read)? else {
        return Ok(());
    };
    visit_conduit_table(&table, visit)
}

pub(crate) fn visit_conduit_table(
    table: &redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
    mut visit: impl FnMut(Address, Address, u64, &[SlopeSegment]) -> eyre::Result<()>,
) -> eyre::Result<()> {
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

impl super::super::catalog::EntityRead {
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
