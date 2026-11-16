use alloy_primitives::{Address, B256};
use routing::SlopeSegment;
use store::service::Snapshot;
use venue::{TokenRecord, VenueRow};

/// Reads one snapshot of the entity file.
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

impl Source for Snapshot {
    fn tokens(&self) -> eyre::Result<Vec<TokenRecord>> {
        self.token_rows()
    }

    fn visit_conduits(
        &self,
        visit: &mut dyn FnMut(Address, Address, &[SlopeSegment]) -> eyre::Result<()>,
    ) -> eyre::Result<()> {
        self.each_conduit(|token_in, token_out, segments| visit(token_in, token_out, segments))
    }

    fn conduit_segments(
        &self,
        token_in: Address,
        token_out: Address,
    ) -> eyre::Result<Option<Vec<SlopeSegment>>> {
        Ok(self
            .conduit_at(token_in, token_out)?
            .map(|stored| stored.segments)
            .filter(|segments| !segments.is_empty()))
    }

    fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        self.venue_row(kind, id)
    }
}
