use alloy_primitives::{Address, B256};
use routing::SlopeSegment;
use venues::{Catalog, EntityRead, TokenRecord, VenueRow};

/// Reads the entity file. The writer and a read-only follower both implement it.
pub(crate) trait Source {
    fn tokens(&self) -> eyre::Result<Vec<TokenRecord>>;
    fn visit_conduits(
        &self,
        visit: &mut dyn FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()>;
    fn conduit_segments(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<Vec<SlopeSegment>>>;
    fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>>;
}

impl Source for Catalog {
    fn tokens(&self) -> eyre::Result<Vec<TokenRecord>> {
        Catalog::tokens(self)
    }

    fn visit_conduits(
        &self,
        visit: &mut dyn FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        Catalog::visit_conduits(self, |token_in, token_out, segments| {
            visit(token_in, token_out, segments)
        })
    }

    fn conduit_segments(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<Vec<SlopeSegment>>> {
        Ok(Catalog::read_conduit(self, token_in, token_out)?
            .map(|stored| stored.segments)
            .filter(|segments| !segments.is_empty()))
    }

    fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        Catalog::read_venue(self, kind, id)
    }
}

impl Source for EntityRead {
    fn tokens(&self) -> eyre::Result<Vec<TokenRecord>> {
        EntityRead::tokens(self)
    }

    fn visit_conduits(
        &self,
        visit: &mut dyn FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        EntityRead::visit_conduits(self, |token_in, token_out, segments| {
            visit(token_in, token_out, segments)
        })
    }

    fn conduit_segments(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<Vec<SlopeSegment>>> {
        Ok(EntityRead::read_conduit(self, token_in, token_out)?
            .map(|stored| stored.segments)
            .filter(|segments| !segments.is_empty()))
    }

    fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        EntityRead::read_venue(self, kind, id)
    }
}
