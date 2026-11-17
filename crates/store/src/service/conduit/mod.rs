//! Pair conduit: the slope merge of every direct action that spends one token
//! and pays another.
//!
//! At input `x` the value is the most those actions can pay together,
//! `F(x) = max { sum f_i(x_i) | x_i >= 0, sum x_i <= x }`. The stored row is
//! that merge. A path through a third token is a composition of these rows.
//! The actions in one row do not share a pool. Each row is built as though it
//! had the pool alone.

mod codec;
mod job;
mod member;
mod meta;
mod read;

pub(crate) use job::account_jobs;
pub(crate) use meta::each_meta;
pub use read::ConduitRow;
pub(crate) use read::{
    conduit_get, conduit_point, conduit_rows, open_conduit_table, visit_conduit_table,
};

use alloy_primitives::{Address, B256};
use redb::{ReadableDatabase, ReadableTableMetadata, TableDefinition};
use routing::SlopeSegment;
use store_model::CurveRecord;

/// The stored slope merge for one ordered pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredConduit {
    pub block: u64,
    pub generation: u64,
    pub members: u32,
    pub segments: Vec<SlopeSegment>,
}

/// Kind mask and width sum of one stored conduit.
///
/// `kinds` has one bit per venue kind, starting at kind 1. `capacity` is the
/// saturating sum of the piece widths. `segments` is the number of pieces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConduitMeta {
    pub kinds: u32,
    pub capacity: u128,
    pub segments: u32,
}

/// One priced direction the member index should hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberTarget {
    pub token_in: Address,
    pub token_out: Address,
    pub kind: u8,
    pub id: B256,
    pub generation: u64,
}

/// One row of the member index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConduitMember {
    pub key: [u8; 73],
    pub kind: u8,
    pub id: B256,
    pub generation: u64,
}

/// One curve key visited while paging. `curve` is absent when the row does not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurveRow {
    pub kind: u8,
    pub id: B256,
    pub curve: Option<CurveRecord>,
}

/// A page of the curves table. `resume` is the last key visited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CurvePage {
    pub rows: Vec<CurveRow>,
    pub resume: Option<(u8, B256)>,
    pub more: bool,
}

/// Slope merge to store when the member index still matches `snapshot`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConduitBody {
    pub token_in: Address,
    pub token_out: Address,
    pub snapshot: Vec<ConduitMember>,
    pub kept: Vec<[u8; 73]>,
    pub block: u64,
    pub generation: u64,
    pub segments: Vec<SlopeSegment>,
}

const CONDUITS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduits");

const META: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_meta");

const MEMBERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_members");

const JOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_jobs");

const CURVES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("curves");

pub(crate) fn count_conduits(db: &redb::Database) -> eyre::Result<u64> {
    count_table(db, CONDUITS)
}

fn pair_key(token_in: Address, token_out: Address) -> [u8; 40] {
    let mut out = [0_u8; 40];
    out[..20].copy_from_slice(token_in.as_slice());
    out[20..].copy_from_slice(token_out.as_slice());
    out
}

fn pair_addresses(pair: &[u8; 40]) -> (Address, Address) {
    (
        Address::from_slice(&pair[..20]),
        Address::from_slice(&pair[20..]),
    )
}

fn member_key(pair: &[u8; 40], kind: u8, id: &B256) -> [u8; 73] {
    let mut out = [0_u8; 73];
    out[..40].copy_from_slice(pair);
    out[40] = kind;
    out[41..].copy_from_slice(id.as_slice());
    out
}

fn split_member(key: &[u8]) -> Option<([u8; 40], u8, B256)> {
    if key.len() != 73 {
        return None;
    }
    let mut pair = [0_u8; 40];
    pair.copy_from_slice(&key[..40]);
    Some((pair, key[40], B256::from_slice(&key[41..])))
}

fn prefix_end(prefix: &[u8; 40]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last != u8::MAX {
            *last += 1;
            return Some(end);
        }
        end.pop();
    }
    None
}

fn open_members(
    read: &redb::ReadTransaction,
) -> eyre::Result<redb::ReadOnlyTable<&'static [u8], &'static [u8]>> {
    match open_bytes(read, MEMBERS)? {
        Some(table) => Ok(table),
        None => Err(eyre::eyre!("conduit members")),
    }
}

fn open_conduits(
    read: &redb::ReadTransaction,
) -> eyre::Result<Option<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    open_bytes(read, CONDUITS)
}

fn open_bytes(
    read: &redb::ReadTransaction,
    def: TableDefinition<&'static [u8], &'static [u8]>,
) -> eyre::Result<Option<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>> {
    match read.open_table(def) {
        Ok(table) => Ok(Some(table)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

/// Row count stored on the tree header. Walking the table reads every leaf.
fn count_table(db: &redb::Database, def: TableDefinition<&[u8], &[u8]>) -> eyre::Result<u64> {
    let read = db.begin_read()?;
    let table = match read.open_table(def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    Ok(table.len()?)
}
