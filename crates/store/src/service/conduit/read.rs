//! Stored pair conduits.

use alloy_primitives::Address;
use redb::{ReadableDatabase, ReadableTable};
use routing::SlopeSegment;

use super::super::catalog::Catalog;
use super::codec::decode_conduit;
use super::*;

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
    db: &impl crate::storage::Handle,
    mut visit: impl FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
) -> eyre::Result<()> {
    visit_conduit_rows(db, |token_in, token_out, _block, segments| {
        visit(token_in, token_out, segments)
    })
}

pub(crate) fn visit_conduit_rows(
    db: &impl crate::storage::Handle,
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
