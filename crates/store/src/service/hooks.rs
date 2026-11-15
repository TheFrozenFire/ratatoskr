//! Domain hooks the catalog calls when a stored row has to be priced.
//!
//! `venues` installs these before it opens a catalog. A missing hook is an
//! error, so a queue decision is not silently changed.

use alloy_primitives::{Address, B256, U256};
use routing::SlopeSegment;
use std::sync::OnceLock;
use store_model::{CurveRecord, PoolState, TickWord};

/// Trade size of one stored snapshot. `top` is the largest priced input.
pub struct Capacity {
    pub any_positive: bool,
    pub fills_sample: bool,
    pub top: U256,
}

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

type CapacityFn = fn(u8, B256, &PoolState) -> Option<Capacity>;
type TradeSizeFn = fn(u8, B256, PoolState) -> bool;
type PairsFn = fn(&CurveRecord) -> Vec<(Address, Address)>;
type SegmentsFn = fn(&DirectionRows<'_>) -> Option<Vec<SlopeSegment>>;

static CAPACITY: OnceLock<CapacityFn> = OnceLock::new();
static TRADE_SIZE: OnceLock<TradeSizeFn> = OnceLock::new();
static PAIRS: OnceLock<PairsFn> = OnceLock::new();
static SEGMENTS: OnceLock<SegmentsFn> = OnceLock::new();

/// Install the snapshot-capacity and empty-pool hooks. A second call is ignored.
pub fn install_capacity(capacity: CapacityFn, trade_size: TradeSizeFn) {
    let _ = CAPACITY.set(capacity);
    let _ = TRADE_SIZE.set(trade_size);
}

/// Install priced-pair and slope-segment hooks. A second call is ignored.
pub fn install_segments(pairs: PairsFn, segments: SegmentsFn) {
    let _ = PAIRS.set(pairs);
    let _ = SEGMENTS.set(segments);
}

pub(crate) fn capacity(kind: u8, id: B256, state: &PoolState) -> eyre::Result<Option<Capacity>> {
    let Some(hook) = CAPACITY.get().copied() else {
        return Err(eyre::eyre!("capacity hook is not installed"));
    };
    Ok(hook(kind, id, state))
}

pub(crate) fn has_trade_size(kind: u8, id: B256, state: PoolState) -> eyre::Result<bool> {
    let Some(hook) = TRADE_SIZE.get().copied() else {
        return Err(eyre::eyre!("trade size hook is not installed"));
    };
    Ok(hook(kind, id, state))
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
