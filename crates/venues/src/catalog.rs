//! Venue identity catalog.
//!
//! One redb file. `venues` is keyed by kind and id. `cursors` is the next
//! index or block a named job still has to read. A chunk writes its rows and
//! its cursor in one transaction.

use alloy_primitives::{Address, B256};
use eyre::WrapErr;
use redb::{Database, Durability, ReadableTable, TableDefinition};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const VENUES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("venues");
const CURSORS: TableDefinition<&str, u64> = TableDefinition::new("cursors");
/// Bloom hit. Key is the job name, `0xff`, then the block number in big-endian.
const HITS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hits");
/// Next block a job has already bloom-tested. Exclusive.
const HIT_SCAN: TableDefinition<&str, u64> = TableDefinition::new("hit_scan");
/// Era1 receipt payload for one block, so a retry does not range-read it again.
const BODIES: TableDefinition<u64, &[u8]> = TableDefinition::new("bodies");
const RECORD_VERSION: u8 = 1;
const FLAG_FEE: u8 = 1;
const FLAG_TICK: u8 = 2;
const FLAG_BLOCK: u8 = 4;
const FLAG_EXTRA: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    /// Fill gaps in an existing row. A later list does not erase a block number.
    Merge,
    /// Leave an existing row alone. Used when a registry repeats a factory pool.
    IfAbsent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VenueRow {
    pub kind: u8,
    pub id: B256,
    pub factory: Address,
    pub tokens: Vec<Address>,
    pub fee: Option<u32>,
    pub tick_spacing: Option<i32>,
    pub block_number: Option<u64>,
    pub extra: Option<Address>,
}

impl VenueRow {
    pub fn address_id(address: Address) -> B256 {
        let mut id = B256::ZERO;
        id[12..].copy_from_slice(address.as_slice());
        id
    }

    fn key(&self) -> [u8; 33] {
        key(self.kind, &self.id)
    }
}

fn key(kind: u8, id: &B256) -> [u8; 33] {
    let mut out = [0u8; 33];
    out[0] = kind;
    out[1..].copy_from_slice(id.as_slice());
    out
}

pub const UNI_V2: u8 = 1;
pub const SUSHI_V2: u8 = 2;
pub const PANCAKE_V2: u8 = 3;
pub const V2_FORK: u8 = 4;
pub const UNI_V3: u8 = 5;
pub const PANCAKE_V3: u8 = 6;
pub const SUSHI_V3: u8 = 7;
pub const V3_FORK: u8 = 8;
pub const AERO: u8 = 9;
pub const UNI_V4: u8 = 10;
pub const BALANCER_V2: u8 = 11;
pub const BALANCER_V3: u8 = 12;
pub const CURVE: u8 = 13;
pub const METRIC: u8 = 14;

pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        1 => "uniswap_v2",
        2 => "sushi_v2",
        3 => "pancake_v2",
        4 => "v2_fork",
        5 => "uniswap_v3",
        6 => "pancake_v3",
        7 => "sushi_v3",
        8 => "v3_fork",
        9 => "aero",
        10 => "uniswap_v4",
        11 => "balancer_v2",
        12 => "balancer_v3",
        13 => "curve",
        14 => "metric",
        _ => "other",
    }
}

pub struct Catalog {
    db: Database,
}

impl Catalog {
    pub fn open(path: &Path) -> eyre::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let db = if path.exists() {
            Database::open(path).wrap_err("open venue catalog")?
        } else {
            Database::create(path).wrap_err("create venue catalog")?
        };
        Ok(Self { db })
    }

    pub fn default_path() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
        PathBuf::from(home).join(".local/share/ratatoskr/venues.redb")
    }

    pub fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(CURSORS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(job)?.map(|value| value.value()))
    }

    /// Write `rows` and set `job` so the next read starts at `next`.
    pub fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64> {
        let mut write = self.db.begin_write()?;
        write.set_durability(Durability::Immediate);
        let written = {
            let mut venues = write.open_table(VENUES)?;
            let mut cursors = write.open_table(CURSORS)?;
            if let Some(stored) = cursors.get(job)?.map(|value| value.value()) {
                if next < stored {
                    return Err(eyre::eyre!("cursor {job} is {stored}; {next} is below it"));
                }
            }
            let mut n = 0_u64;
            for row in rows {
                let key = row.key();
                let previous = venues
                    .get(key.as_slice())?
                    .map(|value| value.value().to_vec());
                let stored = match (previous, put) {
                    (Some(_), Put::IfAbsent) => None,
                    (Some(bytes), Put::Merge) => {
                        let old = decode_row(row.kind, row.id, &bytes)?;
                        Some(encode_row(&merge(old, row.clone())))
                    }
                    (None, _) => Some(encode_row(row)),
                };
                if let Some(bytes) = stored {
                    venues.insert(key.as_slice(), bytes.as_slice())?;
                    n += 1;
                }
            }
            cursors.insert(job, next)?;
            n
        };
        write.commit()?;
        Ok(written)
    }

    /// Next block `job` has already tested against the header blooms.
    pub fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(HIT_SCAN) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(job)?.map(|value| value.value()))
    }

    /// Bloom hits in `[from, to)`.
    pub fn hits_from(&self, job: &str, from: u64, to: u64) -> eyre::Result<Vec<u64>> {
        self.hits_from_limit(job, from, to, usize::MAX)
    }

    /// At most `limit` bloom hits in `[from, to)`, in key order.
    ///
    /// The range stops once `limit` rows have been read.
    pub fn hits_from_limit(
        &self,
        job: &str,
        from: u64,
        to: u64,
        limit: usize,
    ) -> eyre::Result<Vec<u64>> {
        if from >= to || limit == 0 {
            return Ok(Vec::new());
        }
        let read = self.db.begin_read()?;
        let table = match read.open_table(HITS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let start = hit_key(job, from);
        let end = hit_key(job, to);
        let mut hits = Vec::new();
        for item in table.range(start.as_slice()..end.as_slice())? {
            if hits.len() == limit {
                break;
            }
            let (key, _) = item?;
            hits.push(block_of_hit_key(job, key.value())?);
        }
        Ok(hits)
    }

    /// Store `hits` and remember that every block before `scanned_to` was tested.
    ///
    /// A later call with a smaller `scanned_to` does not rewind the scan.
    pub fn commit_hits(&self, job: &str, scanned_to: u64, hits: &[u64]) -> eyre::Result<()> {
        let mut write = self.db.begin_write()?;
        write.set_durability(Durability::Immediate);
        {
            let mut table = write.open_table(HITS)?;
            let mut scan = write.open_table(HIT_SCAN)?;
            for block in hits {
                let key = hit_key(job, *block);
                table.insert(key.as_slice(), &[1u8][..])?;
            }
            let previous = scan.get(job)?.map(|value| value.value()).unwrap_or(0);
            if scanned_to > previous {
                scan.insert(job, scanned_to)?;
            }
        }
        write.commit()?;
        Ok(())
    }

    pub fn read_bodies(&self, numbers: &[u64]) -> eyre::Result<Vec<Option<ReceiptBody>>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(BODIES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Ok(numbers.iter().map(|_| None).collect());
            }
            Err(err) => return Err(err.into()),
        };
        let mut out = Vec::with_capacity(numbers.len());
        for number in numbers {
            let body = match table.get(*number)? {
                Some(value) => Some(decode_body(value.value())?),
                None => None,
            };
            out.push(body);
        }
        Ok(out)
    }

    pub fn put_bodies(&self, bodies: &[(u64, ReceiptBody)]) -> eyre::Result<()> {
        if bodies.is_empty() {
            return Ok(());
        }
        let mut write = self.db.begin_write()?;
        write.set_durability(Durability::Immediate);
        {
            let mut table = write.open_table(BODIES)?;
            for (number, body) in bodies {
                let bytes = encode_body(body);
                table.insert(*number, bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }
}

fn hit_key(job: &str, block: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(job.len() + 1 + 8);
    key.extend_from_slice(job.as_bytes());
    key.push(0xff);
    key.extend_from_slice(&block.to_be_bytes());
    key
}

fn block_of_hit_key(job: &str, key: &[u8]) -> eyre::Result<u64> {
    let prefix = job.len() + 1;
    if key.len() != prefix + 8 || &key[..job.len()] != job.as_bytes() || key[job.len()] != 0xff {
        return Err(eyre::eyre!("hit key"));
    }
    Ok(u64::from_be_bytes(key[prefix..].try_into().unwrap()))
}

/// Era1 receipt record kept so the same block is not fetched again.
///
/// `payload` is the snappy frame. An empty payload is a block with no receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptBody {
    pub hash: B256,
    pub parent_hash: B256,
    pub timestamp: u64,
    pub receipts_root: B256,
    pub payload: Vec<u8>,
}

const BODY_VERSION: u8 = 1;
const BODY_FIXED: usize = 1 + 32 + 32 + 8 + 32;

pub fn encode_body(body: &ReceiptBody) -> Vec<u8> {
    let mut out = Vec::with_capacity(BODY_FIXED + body.payload.len());
    out.push(BODY_VERSION);
    out.extend_from_slice(body.hash.as_slice());
    out.extend_from_slice(body.parent_hash.as_slice());
    out.extend_from_slice(&body.timestamp.to_le_bytes());
    out.extend_from_slice(body.receipts_root.as_slice());
    out.extend_from_slice(&body.payload);
    out
}

pub fn decode_body(bytes: &[u8]) -> eyre::Result<ReceiptBody> {
    if bytes.len() < BODY_FIXED || bytes[0] != BODY_VERSION {
        return Err(eyre::eyre!("receipt body header"));
    }
    Ok(ReceiptBody {
        hash: B256::from_slice(&bytes[1..33]),
        parent_hash: B256::from_slice(&bytes[33..65]),
        timestamp: u64::from_le_bytes(bytes[65..73].try_into().unwrap()),
        receipts_root: B256::from_slice(&bytes[73..105]),
        payload: bytes[BODY_FIXED..].to_vec(),
    })
}

fn factory_of(bytes: &[u8]) -> eyre::Result<Address> {
    if bytes.len() < 22 || bytes[0] != RECORD_VERSION {
        return Err(eyre::eyre!("venue row header"));
    }
    Ok(Address::from_slice(&bytes[2..22]))
}

fn merge(mut old: VenueRow, new: VenueRow) -> VenueRow {
    if new.factory != Address::ZERO {
        old.factory = new.factory;
    }
    if !new.tokens.is_empty() {
        old.tokens = new.tokens;
    }
    if new.fee.is_some() {
        old.fee = new.fee;
    }
    if new.tick_spacing.is_some() {
        old.tick_spacing = new.tick_spacing;
    }
    if new.block_number.is_some() {
        old.block_number = new.block_number;
    }
    if new.extra.is_some() {
        old.extra = new.extra;
    }
    old
}

pub fn encode_row(row: &VenueRow) -> Vec<u8> {
    let mut flags = 0_u8;
    if row.fee.is_some() {
        flags |= FLAG_FEE;
    }
    if row.tick_spacing.is_some() {
        flags |= FLAG_TICK;
    }
    if row.block_number.is_some() {
        flags |= FLAG_BLOCK;
    }
    if row.extra.is_some() {
        flags |= FLAG_EXTRA;
    }
    let tokens = row.tokens.len().min(16);
    let mut out = Vec::with_capacity(2 + 20 + 1 + tokens * 20 + 16);
    out.push(RECORD_VERSION);
    out.push(flags);
    out.extend_from_slice(row.factory.as_slice());
    out.push(tokens as u8);
    for token in row.tokens.iter().take(tokens) {
        out.extend_from_slice(token.as_slice());
    }
    if let Some(fee) = row.fee {
        out.extend_from_slice(&fee.to_le_bytes());
    }
    if let Some(tick) = row.tick_spacing {
        out.extend_from_slice(&tick.to_le_bytes());
    }
    if let Some(block) = row.block_number {
        out.extend_from_slice(&block.to_le_bytes());
    }
    if let Some(extra) = row.extra {
        out.extend_from_slice(extra.as_slice());
    }
    out
}

pub fn decode_row(kind: u8, id: B256, bytes: &[u8]) -> eyre::Result<VenueRow> {
    if bytes.len() < 23 || bytes[0] != RECORD_VERSION {
        return Err(eyre::eyre!("venue row header"));
    }
    let flags = bytes[1];
    let factory = Address::from_slice(&bytes[2..22]);
    let n = bytes[22] as usize;
    let mut at = 23;
    if bytes.len() < at + n * 20 {
        return Err(eyre::eyre!("venue row tokens"));
    }
    let mut tokens = Vec::with_capacity(n);
    for _ in 0..n {
        tokens.push(Address::from_slice(&bytes[at..at + 20]));
        at += 20;
    }
    let fee = if flags & FLAG_FEE != 0 {
        Some(read_u32(bytes, &mut at)?)
    } else {
        None
    };
    let tick_spacing = if flags & FLAG_TICK != 0 {
        Some(read_i32(bytes, &mut at)?)
    } else {
        None
    };
    let block_number = if flags & FLAG_BLOCK != 0 {
        Some(read_u64(bytes, &mut at)?)
    } else {
        None
    };
    let extra = if flags & FLAG_EXTRA != 0 {
        if bytes.len() < at + 20 {
            return Err(eyre::eyre!("venue row extra"));
        }
        let address = Address::from_slice(&bytes[at..at + 20]);
        at += 20;
        Some(address)
    } else {
        None
    };
    if at != bytes.len() {
        return Err(eyre::eyre!("venue row trailing bytes"));
    }
    Ok(VenueRow {
        kind,
        id,
        factory,
        tokens,
        fee,
        tick_spacing,
        block_number,
        extra,
    })
}

fn read_u32(bytes: &[u8], at: &mut usize) -> eyre::Result<u32> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| eyre::eyre!("venue row fee"))?;
    let word = bytes
        .get(*at..end)
        .ok_or_else(|| eyre::eyre!("venue row fee"))?;
    *at = end;
    Ok(u32::from_le_bytes(word.try_into().unwrap()))
}

fn read_i32(bytes: &[u8], at: &mut usize) -> eyre::Result<i32> {
    let end = at
        .checked_add(4)
        .ok_or_else(|| eyre::eyre!("venue row tick"))?;
    let word = bytes
        .get(*at..end)
        .ok_or_else(|| eyre::eyre!("venue row tick"))?;
    *at = end;
    Ok(i32::from_le_bytes(word.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], at: &mut usize) -> eyre::Result<u64> {
    let end = at
        .checked_add(8)
        .ok_or_else(|| eyre::eyre!("venue row block"))?;
    let word = bytes
        .get(*at..end)
        .ok_or_else(|| eyre::eyre!("venue row block"))?;
    *at = end;
    Ok(u64::from_le_bytes(word.try_into().unwrap()))
}

/// Kind is stored in the key. The tally has to read it from there.
pub fn tally_kind(key: &[u8]) -> Option<u8> {
    key.first().copied()
}

impl Catalog {
    pub fn tally_by_kind(&self) -> eyre::Result<BTreeMap<(u8, Address), u64>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(VENUES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
            Err(err) => return Err(err.into()),
        };
        let mut counts = BTreeMap::new();
        for row in table.iter()? {
            let (key, value) = row?;
            let Some(kind) = tally_kind(key.value()) else {
                continue;
            };
            let factory = factory_of(value.value())?;
            *counts.entry((kind, factory)).or_default() += 1;
        }
        Ok(counts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn sample() -> VenueRow {
        VenueRow {
            kind: 1,
            id: VenueRow::address_id(address!("1111111111111111111111111111111111111111")),
            factory: address!("2222222222222222222222222222222222222222"),
            tokens: vec![
                address!("3333333333333333333333333333333333333333"),
                address!("4444444444444444444444444444444444444444"),
            ],
            fee: Some(3000),
            tick_spacing: Some(-10),
            block_number: Some(12_345),
            extra: Some(address!("5555555555555555555555555555555555555555")),
        }
    }

    #[test]
    fn row_codec_roundtrip() {
        let row = sample();
        let bytes = encode_row(&row);
        let back = decode_row(row.kind, row.id, &bytes).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn merge_keeps_a_block_when_the_new_row_has_none() {
        let old = sample();
        let mut new = sample();
        new.block_number = None;
        new.tokens.clear();
        new.fee = Some(100);
        let merged = merge(old.clone(), new);
        assert_eq!(merged.block_number, old.block_number);
        assert_eq!(merged.tokens, old.tokens);
        assert_eq!(merged.fee, Some(100));
    }

    #[test]
    fn commit_is_resumable_and_if_absent_does_not_replace() {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-venues-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let catalog = Catalog::open(&path).unwrap();
        let row = sample();
        catalog
            .commit_chunk("list:test", 10, &[row.clone()], Put::Merge)
            .unwrap();
        assert_eq!(catalog.cursor("list:test").unwrap(), Some(10));
        let mut other = row.clone();
        other.factory = address!("6666666666666666666666666666666666666666");
        let wrote = catalog
            .commit_chunk("list:test", 11, &[other], Put::IfAbsent)
            .unwrap();
        assert_eq!(wrote, 0);
        let counts = catalog.tally_by_kind().unwrap();
        assert_eq!(counts.get(&(1, row.factory)).copied(), Some(1));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn hits_and_bodies_survive_a_reopen() {
        let path = std::env::temp_dir().join(format!(
            "ratatoskr-venues-hits-{}-{}.redb",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&path);
        let body = ReceiptBody {
            hash: alloy_primitives::b256!(
                "1111111111111111111111111111111111111111111111111111111111111111"
            ),
            parent_hash: B256::ZERO,
            timestamp: 9,
            receipts_root: B256::ZERO,
            payload: vec![1, 2, 3],
        };
        {
            let catalog = Catalog::open(&path).unwrap();
            assert!(catalog.hit_scan("logs:test").unwrap().is_none());
            catalog.commit_hits("logs:test", 10, &[3, 4]).unwrap();
            catalog.commit_hits("logs:test", 8, &[1]).unwrap();
            catalog.put_bodies(&[(4, body.clone())]).unwrap();
        }
        let catalog = Catalog::open(&path).unwrap();
        assert_eq!(catalog.hit_scan("logs:test").unwrap(), Some(10));
        assert_eq!(
            catalog.hits_from("logs:test", 0, 10).unwrap(),
            vec![1, 3, 4]
        );
        assert_eq!(catalog.hits_from("logs:test", 4, 5).unwrap(), vec![4]);
        let stored = catalog.read_bodies(&[4, 5]).unwrap();
        assert_eq!(stored[0].as_ref(), Some(&body));
        assert!(stored[1].is_none());
        drop(catalog);
        let _ = std::fs::remove_file(&path);
    }
}
