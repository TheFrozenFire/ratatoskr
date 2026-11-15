//! Priced pairs and slope segments.

use alloy_primitives::{Address, B256};
use routing::SlopeSegment;
use std::sync::OnceLock;
use store_model::{CurveRecord, TickWord};

/// Stored rows one ordered pair is priced from.
pub struct DirectionRows<'a> {
    pub immutable: &'a [u8],
    pub mutable_body: &'a [u8],
    pub ticks: &'a [TickWord],
    pub kind: u8,
    pub id: B256,
    pub curve: &'a CurveRecord,
    pub token_in: Address,
    pub token_out: Address,
}

type PairsFn = fn(&CurveRecord) -> Vec<(Address, Address)>;
type SegmentsFn = fn(&DirectionRows<'_>) -> Option<Vec<SlopeSegment>>;

static PAIRS: OnceLock<PairsFn> = OnceLock::new();
static SEGMENTS: OnceLock<SegmentsFn> = OnceLock::new();

/// Install priced-pair and slope-segment hooks. A second call is ignored.
pub fn install_segments(pairs: PairsFn, segments: SegmentsFn) {
    let _ = PAIRS.set(pairs);
    let _ = SEGMENTS.set(segments);
}

pub(crate) fn priced_pairs(curve: &CurveRecord) -> eyre::Result<Vec<(Address, Address)>> {
    let Some(hook) = PAIRS.get().copied() else {
        return Err(eyre::eyre!("priced pairs hook is not installed"));
    };
    Ok(hook(curve))
}

pub(crate) fn segments(rows: &DirectionRows<'_>) -> eyre::Result<Option<Vec<SlopeSegment>>> {
    let Some(hook) = SEGMENTS.get().copied() else {
        return Err(eyre::eyre!("segment hook is not installed"));
    };
    Ok(hook(rows))
}
