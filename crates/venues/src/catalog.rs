//! Venue catalog across three redb files.
//!
//! `entities.redb` holds identities and later pool state. `discovery.redb`
//! holds list cursors, bloom hits, and era1 receipt bodies. `work.redb` holds
//! only leases. redb cannot commit across files. Identities are written
//! first and the cursor second, so a crash retries the chunk instead of
//! skipping it. Deleting `work.redb` drops leases, not rows.
//!
//! The identity table keeps the name `venues`. A directory that still has
//! `venues.redb` and no `entities.redb` is renamed, and the crawl tables are
//! copied into `discovery.redb`. An existing `entities.redb` is left in place
//! and the old file is not read again.

use alloy_primitives::{Address, B256};
use eyre::WrapErr;
use redb::{Database, Durability, ReadableTable, TableDefinition};
use std::collections::{BTreeMap, HashSet};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::str::FromStr;

pub(crate) const VENUES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("venues");
const CURSORS: TableDefinition<&str, u64> = TableDefinition::new("cursors");
/// Bloom hit. Key is the job name, `0xff`, then the block number in big-endian.
const HITS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("hits");
/// Next block a job has already bloom-tested. Exclusive.
const HIT_SCAN: TableDefinition<&str, u64> = TableDefinition::new("hit_scan");
/// Era1 receipt payload for one block, so a retry does not range-read it again.
const BODIES: TableDefinition<u64, &[u8]> = TableDefinition::new("bodies");
const RECORD_VERSION: u8 = 1;
const RECORD_VERSION_OBS: u8 = 2;
const FLAG_FEE: u8 = 1;
const FLAG_TICK: u8 = 2;
const FLAG_BLOCK: u8 = 4;
const FLAG_EXTRA: u8 = 8;
const FLAG_OBS: u8 = 16;

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
    pub block_hash: Option<B256>,
    pub tx_index: Option<u64>,
    pub log_index: Option<u64>,
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

pub(crate) fn entity_key(kind: u8, id: &B256) -> [u8; 33] {
    key(kind, id)
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
    pub(crate) discovery: Database,
    pub(crate) entities: Database,
    pub(crate) work: Database,
}

impl Catalog {
    /// Open the catalog directory.
    ///
    /// A path whose file name ends in `.redb` selects that file's parent, so
    /// existing callers that pass `venues.redb` keep the same directory.
    pub fn open(path: &Path) -> eyre::Result<Self> {
        let dir = catalog_dir(path);
        std::fs::create_dir_all(&dir)?;
        migrate(&dir)?;
        Ok(Self {
            discovery: open_db(&dir.join("discovery.redb"))?,
            entities: open_db(&dir.join("entities.redb"))?,
            work: open_db(&dir.join("work.redb"))?,
        })
    }

    pub fn default_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr/entities.redb")
    }

    pub fn cursor(&self, job: &str) -> eyre::Result<Option<u64>> {
        let read = self.discovery.begin_read()?;
        let table = match read.open_table(CURSORS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        Ok(table.get(job)?.map(|value| value.value()))
    }

    /// Write identities, then set `job` so the next read starts at `next`.
    ///
    /// The two files commit separately. A crash after the identities and
    /// before the cursor runs this chunk again. Merge is idempotent.
    pub fn commit_chunk(
        &self,
        job: &str,
        next: u64,
        rows: &[VenueRow],
        put: Put,
    ) -> eyre::Result<u64> {
        if let Some(stored) = self.cursor(job)? {
            if next < stored {
                return Err(eyre::eyre!("cursor {job} is {stored}; {next} is below it"));
            }
        }
        let written = write_identities(&self.entities, rows, put)?;
        let mut write = self.discovery.begin_write()?;
        write.set_durability(Durability::Immediate);
        {
            let mut cursors = write.open_table(CURSORS)?;
            if let Some(stored) = cursors.get(job)?.map(|value| value.value()) {
                if next < stored {
                    return Err(eyre::eyre!("cursor {job} is {stored}; {next} is below it"));
                }
            }
            cursors.insert(job, next)?;
        }
        write.commit()?;
        Ok(written)
    }

    /// Next block `job` has already tested against the header blooms.
    pub fn hit_scan(&self, job: &str) -> eyre::Result<Option<u64>> {
        let read = self.discovery.begin_read()?;
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
        let read = self.discovery.begin_read()?;
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
        let mut write = self.discovery.begin_write()?;
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
        let read = self.discovery.begin_read()?;
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
        let mut write = self.discovery.begin_write()?;
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

    /// At most `limit` rows of `kind` whose id is greater than `after`.
    ///
    /// The read stops when the page is full. Later keys are not visited.
    pub fn page_venues(
        &self,
        kind: u8,
        after: Option<B256>,
        limit: usize,
    ) -> eyre::Result<Vec<VenueRow>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let read = self.entities.begin_read()?;
        let table = match read.open_table(VENUES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let start_key = match after {
            Some(id) => key(kind, &id),
            None => key(kind, &B256::ZERO),
        };
        let mut end_key = [0u8; 33];
        if kind != u8::MAX {
            end_key[0] = kind + 1;
        }
        let start = match after {
            Some(_) => Bound::Excluded(start_key.as_slice()),
            None => Bound::Included(start_key.as_slice()),
        };
        let end = if kind == u8::MAX {
            Bound::Unbounded
        } else {
            Bound::Excluded(end_key.as_slice())
        };
        let mut page = Vec::with_capacity(limit);
        for item in table.range::<&[u8]>((start, end))? {
            if page.len() == limit {
                break;
            }
            let (key_bytes, value) = item?;
            let key_bytes = key_bytes.value();
            if key_bytes.len() != 33 || key_bytes[0] != kind {
                return Err(eyre::eyre!("venue key"));
            }
            let id = B256::from_slice(&key_bytes[1..]);
            page.push(decode_row(kind, id, value.value())?);
        }
        Ok(page)
    }
}

fn catalog_dir(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if name.ends_with(".redb") {
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    } else {
        path.to_path_buf()
    }
}

fn open_db(path: &Path) -> eyre::Result<Database> {
    if path.exists() {
        Database::open(path).wrap_err_with(|| format!("open {}", path.display()))
    } else {
        Database::create(path).wrap_err_with(|| format!("create {}", path.display()))
    }
}

/// Move a legacy `venues.redb` onto the three-file layout.
///
/// Identities stay where they are: the file is renamed to `entities.redb`.
/// Crawl tables are copied into `discovery.redb` and then removed. When
/// `entities.redb` is already present, `venues.redb` is not opened.
fn migrate(dir: &Path) -> eyre::Result<()> {
    let venues = dir.join("venues.redb");
    let entities = dir.join("entities.redb");
    let discovery = dir.join("discovery.redb");
    if entities.exists() {
        // `venues.redb` beside an existing `entities.redb` is the pre-migration
        // file. Leave it closed so a second open cannot copy it again.
        if venues.exists() {
            return Ok(());
        }
        if !discovery.exists() {
            let from = Database::open(&entities).wrap_err("open entities for crawl copy")?;
            let to = open_db(&discovery)?;
            copy_crawl(&from, &to)?;
            drop(from);
        }
        let entities = Database::open(&entities).wrap_err("open entities")?;
        delete_crawl(&entities)?;
        return Ok(());
    }
    if !venues.exists() {
        return Ok(());
    }
    let from = Database::open(&venues).wrap_err("open venues.redb")?;
    let to = open_db(&discovery)?;
    copy_crawl(&from, &to)?;
    drop(from);
    drop(to);
    std::fs::rename(&venues, &entities).wrap_err("rename venues.redb to entities.redb")?;
    let entities = Database::open(&entities).wrap_err("open renamed entities")?;
    delete_crawl(&entities)?;
    Ok(())
}

fn copy_crawl(from: &Database, to: &Database) -> eyre::Result<()> {
    copy_str_u64(from, to, CURSORS)?;
    copy_str_u64(from, to, HIT_SCAN)?;
    copy_bytes(from, to, HITS)?;
    copy_u64_bytes(from, to, BODIES)?;
    Ok(())
}

fn copy_str_u64(
    from: &Database,
    to: &Database,
    table: TableDefinition<&str, u64>,
) -> eyre::Result<()> {
    let read = from.begin_read()?;
    let source = match read.open_table(table) {
        Ok(source) => source,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut write = to.begin_write()?;
    write.set_durability(Durability::Immediate);
    {
        let mut dest = write.open_table(table)?;
        for item in source.iter()? {
            let (key, value) = item?;
            dest.insert(key.value(), value.value())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn copy_bytes(
    from: &Database,
    to: &Database,
    table: TableDefinition<&[u8], &[u8]>,
) -> eyre::Result<()> {
    let read = from.begin_read()?;
    let source = match read.open_table(table) {
        Ok(source) => source,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut write = to.begin_write()?;
    write.set_durability(Durability::Immediate);
    {
        let mut dest = write.open_table(table)?;
        for item in source.iter()? {
            let (key, value) = item?;
            dest.insert(key.value(), value.value())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn copy_u64_bytes(
    from: &Database,
    to: &Database,
    table: TableDefinition<u64, &[u8]>,
) -> eyre::Result<()> {
    let read = from.begin_read()?;
    let source = match read.open_table(table) {
        Ok(source) => source,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    let mut write = to.begin_write()?;
    write.set_durability(Durability::Immediate);
    {
        let mut dest = write.open_table(table)?;
        for item in source.iter()? {
            let (key, value) = item?;
            dest.insert(key.value(), value.value())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn delete_crawl(db: &Database) -> eyre::Result<()> {
    let mut write = db.begin_write()?;
    write.set_durability(Durability::Immediate);
    write
        .delete_table(CURSORS)
        .map_err(|err| eyre::eyre!("delete cursors: {err}"))?;
    write
        .delete_table(HITS)
        .map_err(|err| eyre::eyre!("delete hits: {err}"))?;
    write
        .delete_table(HIT_SCAN)
        .map_err(|err| eyre::eyre!("delete hit scan: {err}"))?;
    write
        .delete_table(BODIES)
        .map_err(|err| eyre::eyre!("delete bodies: {err}"))?;
    write.commit()?;
    Ok(())
}

fn write_identities(db: &Database, rows: &[VenueRow], put: Put) -> eyre::Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let mut write = db.begin_write()?;
    write.set_durability(Durability::Immediate);
    let written = {
        let mut venues = write.open_table(VENUES)?;
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
                    Some(encode_row(&merge(old, row.clone()))?)
                }
                (None, _) => Some(encode_row(row)?),
            };
            if let Some(bytes) = stored {
                venues.insert(key.as_slice(), bytes.as_slice())?;
                n += 1;
            }
        }
        n
    };
    write.commit()?;
    Ok(written)
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
    if bytes.len() < 22 || !matches!(bytes[0], RECORD_VERSION | RECORD_VERSION_OBS) {
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
    if new.block_hash.is_some() && new.tx_index.is_some() && new.log_index.is_some() {
        old.block_hash = new.block_hash;
        old.tx_index = new.tx_index;
        old.log_index = new.log_index;
    }
    old
}

pub fn encode_row(row: &VenueRow) -> eyre::Result<Vec<u8>> {
    let observation = match (row.block_hash, row.tx_index, row.log_index) {
        (None, None, None) => None,
        (Some(hash), Some(tx_index), Some(log_index)) => Some((hash, tx_index, log_index)),
        _ => return Err(eyre::eyre!("venue row observation")),
    };
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
    if observation.is_some() {
        flags |= FLAG_OBS;
    }
    let tokens = row.tokens.len().min(16);
    let mut out = Vec::with_capacity(2 + 20 + 1 + tokens * 20 + 16 + 48);
    // No observation keeps version 1, so those rows stay the bytes they are today.
    out.push(if observation.is_some() {
        RECORD_VERSION_OBS
    } else {
        RECORD_VERSION
    });
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
    if let Some((hash, tx_index, log_index)) = observation {
        out.extend_from_slice(hash.as_slice());
        out.extend_from_slice(&tx_index.to_le_bytes());
        out.extend_from_slice(&log_index.to_le_bytes());
    }
    Ok(out)
}

pub fn decode_row(kind: u8, id: B256, bytes: &[u8]) -> eyre::Result<VenueRow> {
    if bytes.len() < 23 {
        return Err(eyre::eyre!("venue row header"));
    }
    let version = bytes[0];
    let flags = bytes[1];
    let observed = flags & FLAG_OBS != 0;
    match (version, observed) {
        (RECORD_VERSION, false) | (RECORD_VERSION_OBS, true) => {}
        (RECORD_VERSION, true) | (RECORD_VERSION_OBS, false) => {
            return Err(eyre::eyre!("venue row observation"));
        }
        _ => return Err(eyre::eyre!("venue row header")),
    }
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
    let (block_hash, tx_index, log_index) = if observed {
        if bytes.len() < at + 32 {
            return Err(eyre::eyre!("venue row observation"));
        }
        let hash = B256::from_slice(&bytes[at..at + 32]);
        at += 32;
        let tx_index = read_u64(bytes, &mut at)?;
        let log_index = read_u64(bytes, &mut at)?;
        (Some(hash), Some(tx_index), Some(log_index))
    } else {
        (None, None, None)
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
        block_hash,
        tx_index,
        log_index,
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
        let read = self.entities.begin_read()?;
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

/// How many identities a sample copied, by kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SampleReport {
    pub copied: u64,
    pub by_kind: BTreeMap<u8, u64>,
    /// The USDC/WETH 0.3% pool was in the source and is in the copy.
    pub anchor: bool,
}

/// Copy a varied subset of identities from `src` into the catalog at `dest`.
///
/// `src` is opened as a plain redb file. This does not migrate it. The live
/// `venues.redb` and the live catalog directory are refused.
pub fn sample_identities(src: &Path, dest: &Path, target: u64) -> eyre::Result<SampleReport> {
    if target == 0 {
        return Err(eyre::eyre!("sample target is 0"));
    }
    refuse_live_file(src)?;
    let dest_dir = catalog_dir(dest);
    std::fs::create_dir_all(&dest_dir)?;
    let src_dir = src.parent().unwrap_or_else(|| Path::new("."));
    if dirs_equal(src_dir, &dest_dir)? {
        return Err(eyre::eyre!(
            "sample source and destination are the same directory"
        ));
    }
    refuse_live_dir(&dest_dir)?;
    let source = Database::open(src).wrap_err("open identity source")?;
    let counts = count_identities(&source)?;
    let plan = allocate_sample(&counts, target);
    let catalog = Catalog::open(&dest_dir)?;
    let anchor = anchor_id();
    let read = source.begin_read()?;
    let table = read.open_table(VENUES).wrap_err("identity table")?;
    let mut copied = 0_u64;
    let mut by_kind = BTreeMap::new();
    let mut found_anchor = false;
    for (kind, want) in plan {
        if want == 0 {
            continue;
        }
        let count = counts.get(&kind).copied().unwrap_or(0);
        let step = (count / want).max(1);
        tracing::info!(kind = kind_name(kind), count, want, step, "sampling kind");
        let start_key = key(kind, &B256::ZERO);
        let mut end_key = [0u8; 33];
        let end = if kind == u8::MAX {
            Bound::Unbounded
        } else {
            end_key[0] = kind + 1;
            Bound::Excluded(end_key.as_slice())
        };
        let mut picked = 0_u64;
        let mut wrote = 0_u64;
        let mut chunk = Vec::new();
        for (index, item) in table
            .range::<&[u8]>((Bound::Included(start_key.as_slice()), end))?
            .enumerate()
        {
            let index = index as u64;
            let (key_bytes, value) = item?;
            let key_bytes = key_bytes.value();
            if key_bytes.len() != 33 || key_bytes[0] != kind {
                return Err(eyre::eyre!("venue key"));
            }
            let id = B256::from_slice(&key_bytes[1..]);
            let is_anchor = kind == UNI_V3 && id == anchor;
            let on_stride = picked < want && index.is_multiple_of(step);
            if !on_stride && !is_anchor {
                continue;
            }
            if !is_anchor && picked >= want {
                continue;
            }
            let row = decode_row(kind, id, value.value())?;
            if is_anchor {
                found_anchor = true;
            }
            picked += 1;
            chunk.push(row);
            if chunk.len() == 512 {
                wrote += write_identities(&catalog.entities, &chunk, Put::IfAbsent)?;
                chunk.clear();
            }
            if picked >= want && (kind != UNI_V3 || found_anchor) {
                break;
            }
        }
        if !chunk.is_empty() {
            wrote += write_identities(&catalog.entities, &chunk, Put::IfAbsent)?;
        }
        if wrote > 0 {
            by_kind.insert(kind, wrote);
            copied += wrote;
        }
    }
    Ok(SampleReport {
        copied,
        by_kind,
        anchor: found_anchor,
    })
}

/// Identities `fill_identities` added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FillReport {
    pub copied: u64,
    pub by_kind: BTreeMap<u8, u64>,
    pub ids: Vec<(u8, B256)>,
}

/// Copy `add` identities that are not already in `dest`.
///
/// `src` is opened as a plain redb file. This does not migrate it. Kinds that
/// are already fully copied contribute nothing. The live `venues.redb` is refused.
pub fn fill_identities(src: &Path, dest: &Path, add: u64) -> eyre::Result<FillReport> {
    if add == 0 {
        return Err(eyre::eyre!("fill target is 0"));
    }
    refuse_live_file(src)?;
    let dest_dir = catalog_dir(dest);
    std::fs::create_dir_all(&dest_dir)?;
    let src_dir = src.parent().unwrap_or_else(|| Path::new("."));
    if dirs_equal(src_dir, &dest_dir)? {
        return Err(eyre::eyre!(
            "sample source and destination are the same directory"
        ));
    }
    refuse_live_dir(&dest_dir)?;
    let source = Database::open(src).wrap_err("open identity source")?;
    let source_counts = count_identities(&source)?;
    let catalog = Catalog::open(&dest_dir)?;
    let mut present = identity_keys(&catalog)?;
    drop(catalog);
    remember_dropped(&dest_dir, &mut present)?;
    let mut remaining = BTreeMap::new();
    for (kind, count) in &source_counts {
        let have = present.iter().filter(|key| key[0] == *kind).count() as u64;
        let left = count.saturating_sub(have);
        if left > 0 {
            remaining.insert(*kind, left);
        }
    }
    let plan = allocate_sample(&remaining, add);
    let catalog = Catalog::open(&dest_dir)?;
    let read = source.begin_read()?;
    let table = read.open_table(VENUES).wrap_err("identity table")?;
    let mut copied = 0_u64;
    let mut by_kind = BTreeMap::new();
    let mut ids = Vec::new();
    for (kind, want) in plan {
        if want == 0 {
            continue;
        }
        let count = remaining.get(&kind).copied().unwrap_or(0);
        let step = (count / want).max(1);
        tracing::info!(kind = kind_name(kind), count, want, step, "filling kind");
        let start_key = key(kind, &B256::ZERO);
        let mut end_key = [0u8; 33];
        let end = if kind == u8::MAX {
            Bound::Unbounded
        } else {
            end_key[0] = kind + 1;
            Bound::Excluded(end_key.as_slice())
        };
        let mut seen = 0_u64;
        let mut picked = 0_u64;
        let mut wrote = 0_u64;
        let mut chunk = Vec::new();
        for item in table.range::<&[u8]>((Bound::Included(start_key.as_slice()), end))? {
            let (key_bytes, value) = item?;
            let key_bytes = key_bytes.value();
            if key_bytes.len() != 33 || key_bytes[0] != kind {
                return Err(eyre::eyre!("venue key"));
            }
            let mut owned = [0u8; 33];
            owned.copy_from_slice(key_bytes);
            if present.contains(&owned) {
                continue;
            }
            let take = picked < want && seen.is_multiple_of(step);
            seen += 1;
            if !take {
                continue;
            }
            let id = B256::from_slice(&key_bytes[1..]);
            let row = decode_row(kind, id, value.value())?;
            picked += 1;
            chunk.push(row);
            ids.push((kind, id));
            if chunk.len() == 512 {
                wrote += write_identities(&catalog.entities, &chunk, Put::IfAbsent)?;
                chunk.clear();
            }
            if picked >= want {
                break;
            }
        }
        if !chunk.is_empty() {
            wrote += write_identities(&catalog.entities, &chunk, Put::IfAbsent)?;
        }
        if wrote > 0 {
            by_kind.insert(kind, wrote);
            copied += wrote;
        }
    }
    Ok(FillReport {
        copied,
        by_kind,
        ids,
    })
}

/// Take small kinds whole, then split whatever remains across the large ones.
fn allocate_sample(counts: &BTreeMap<u8, u64>, target: u64) -> BTreeMap<u8, u64> {
    if target == 0 || counts.is_empty() {
        return BTreeMap::new();
    }
    let min_each = (target / counts.len() as u64 / 2).max(1);
    let mut ordered: Vec<(u8, u64)> = counts.iter().map(|(kind, count)| (*kind, *count)).collect();
    ordered.sort_by_key(|(_, count)| *count);
    let mut take = BTreeMap::new();
    let mut used = 0_u64;
    let mut large = Vec::new();
    for (index, (kind, count)) in ordered.iter().copied().enumerate() {
        let remaining = (ordered.len() - index - 1) as u64;
        let room_after = target.saturating_sub(used).saturating_sub(count);
        let need = min_each.saturating_mul(remaining);
        if count > 0 && room_after >= need && used.saturating_add(count) <= target {
            take.insert(kind, count);
            used += count;
        } else if count > 0 {
            large.push((kind, count));
        }
    }
    let mut left = target.saturating_sub(used);
    while left > 0 {
        let open: Vec<usize> = large
            .iter()
            .enumerate()
            .filter(|(_, (kind, count))| take.get(kind).copied().unwrap_or(0) < *count)
            .map(|(index, _)| index)
            .collect();
        if open.is_empty() {
            break;
        }
        let share = (left / open.len() as u64).max(1);
        let mut progressed = false;
        for index in open {
            if left == 0 {
                break;
            }
            let (kind, count) = large[index];
            let have = take.get(&kind).copied().unwrap_or(0);
            let add = share.min(count - have).min(left);
            if add > 0 {
                take.insert(kind, have + add);
                left -= add;
                progressed = true;
            }
        }
        if !progressed {
            break;
        }
    }
    take
}

fn remember_dropped(dir: &Path, present: &mut HashSet<[u8; 33]>) -> eyre::Result<()> {
    let path = dir.join("dropped-empty.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).wrap_err("read dropped empty ids"),
    };
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let kind = parts
            .next()
            .ok_or_else(|| eyre::eyre!("dropped id"))?
            .parse::<u8>()
            .map_err(|err| eyre::eyre!("dropped kind: {err}"))?;
        let id = parts
            .next()
            .ok_or_else(|| eyre::eyre!("dropped id"))?
            .parse::<B256>()
            .map_err(|err| eyre::eyre!("dropped id: {err}"))?;
        present.insert(key(kind, &id));
    }
    Ok(())
}

pub fn append_dropped_ids(dir: &Path, ids: &[(u8, B256)]) -> eyre::Result<()> {
    append_ids(dir, "dropped-empty.txt", ids)
}

pub fn append_fill_ids(dir: &Path, ids: &[(u8, B256)]) -> eyre::Result<()> {
    append_ids(dir, "fill-added.txt", ids)
}

fn append_ids(dir: &Path, name: &str, ids: &[(u8, B256)]) -> eyre::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(name))
        .wrap_err("append ids")?;
    for (kind, id) in ids {
        use std::io::Write;
        writeln!(file, "{kind} {id}").wrap_err("write fill id")?;
    }
    Ok(())
}

pub fn read_fill_ids(dir: &Path) -> eyre::Result<Vec<(u8, B256)>> {
    let path = dir.join("fill-added.txt");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).wrap_err("read fill ids"),
    };
    let mut ids = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let kind = parts
            .next()
            .ok_or_else(|| eyre::eyre!("fill id"))?
            .parse::<u8>()
            .map_err(|err| eyre::eyre!("fill kind: {err}"))?;
        let id = parts
            .next()
            .ok_or_else(|| eyre::eyre!("fill id"))?
            .parse::<B256>()
            .map_err(|err| eyre::eyre!("fill id: {err}"))?;
        ids.push((kind, id));
    }
    Ok(ids)
}

pub fn write_fill_ids(dir: &Path, ids: &[(u8, B256)]) -> eyre::Result<()> {
    let mut text = String::new();
    for (kind, id) in ids {
        text.push_str(&format!("{kind} {id}\n"));
    }
    std::fs::write(dir.join("fill-added.txt"), text).wrap_err("rewrite fill ids")
}

fn identity_keys(catalog: &Catalog) -> eyre::Result<HashSet<[u8; 33]>> {
    let read = catalog.entities.begin_read()?;
    let table = match read.open_table(VENUES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HashSet::new()),
        Err(err) => return Err(err.into()),
    };
    let mut keys = HashSet::new();
    for item in table.iter()? {
        let (key, _) = item?;
        let key = key.value();
        if key.len() != 33 {
            return Err(eyre::eyre!("venue key"));
        }
        let mut owned = [0u8; 33];
        owned.copy_from_slice(key);
        keys.insert(owned);
    }
    Ok(keys)
}

fn count_identities(db: &Database) -> eyre::Result<BTreeMap<u8, u64>> {
    let read = db.begin_read()?;
    let table = match read.open_table(VENUES) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(err) => return Err(err.into()),
    };
    let mut counts = BTreeMap::new();
    for (seen, item) in table.iter()?.enumerate() {
        if seen.is_multiple_of(100_000) {
            tracing::info!(seen, "counting identities");
        }
        let (key, _) = item?;
        let key = key.value();
        if key.len() != 33 {
            return Err(eyre::eyre!("venue key"));
        }
        *counts.entry(key[0]).or_default() += 1;
    }
    tracing::info!(rows = counts.values().sum::<u64>(), "counted identities");
    Ok(counts)
}

fn anchor_id() -> B256 {
    let address = Address::from_str("8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8").expect("anchor");
    VenueRow::address_id(address)
}

fn live_share_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/ratatoskr")
}

fn dirs_equal(left: &Path, right: &Path) -> eyre::Result<bool> {
    if !left.exists() || !right.exists() {
        return Ok(false);
    }
    Ok(std::fs::canonicalize(left)? == std::fs::canonicalize(right)?)
}

fn refuse_live_dir(dir: &Path) -> eyre::Result<()> {
    if dirs_equal(dir, &live_share_dir())? {
        return Err(eyre::eyre!("refusing to write the live catalog"));
    }
    Ok(())
}

fn refuse_live_file(path: &Path) -> eyre::Result<()> {
    let live = live_share_dir().join("venues.redb");
    if path.exists()
        && live.exists()
        && std::fs::canonicalize(path)? == std::fs::canonicalize(&live)?
    {
        return Err(eyre::eyre!("refusing to open the live venues.redb"));
    }
    Ok(())
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
            block_hash: None,
            tx_index: None,
            log_index: None,
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-catalog-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn history() -> VenueRow {
        let mut row = sample();
        row.block_hash = Some(alloy_primitives::b256!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        row.tx_index = Some(4);
        row.log_index = Some(9);
        row
    }

    #[test]
    fn row_codec_roundtrip() {
        let row = sample();
        let bytes = encode_row(&row).unwrap();
        let back = decode_row(row.kind, row.id, &bytes).unwrap();
        assert_eq!(back, row);
    }

    #[test]
    fn a_row_without_an_observation_stays_version_one() {
        let row = sample();
        let bytes = encode_row(&row).unwrap();
        assert_eq!(bytes[0], RECORD_VERSION);
        assert_eq!(bytes[1] & FLAG_OBS, 0);
        assert_eq!(decode_row(row.kind, row.id, &bytes).unwrap(), row);
    }

    #[test]
    fn a_history_row_is_version_two() {
        let row = history();
        let bytes = encode_row(&row).unwrap();
        assert_eq!(bytes[0], RECORD_VERSION_OBS);
        assert_ne!(bytes[1] & FLAG_OBS, 0);
        let back = decode_row(row.kind, row.id, &bytes).unwrap();
        assert_eq!(back.block_hash, row.block_hash);
        assert_eq!(back.tx_index, row.tx_index);
        assert_eq!(back.log_index, row.log_index);
        assert_eq!(back, row);
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(decode_row(row.kind, row.id, &trailing).is_err());
        let mut bare = encode_row(&sample()).unwrap();
        bare[0] = RECORD_VERSION_OBS;
        assert!(decode_row(row.kind, row.id, &bare).is_err());
    }

    #[test]
    fn version_one_with_the_observation_flag_fails() {
        let row = sample();
        let mut bytes = encode_row(&row).unwrap();
        bytes[1] |= FLAG_OBS;
        assert!(decode_row(row.kind, row.id, &bytes).is_err());
    }

    #[test]
    fn merge_keeps_a_stored_observation_when_the_new_row_has_none() {
        let stored = history();
        let bytes = encode_row(&stored).unwrap();
        assert_eq!(bytes[0], RECORD_VERSION_OBS);
        let old = decode_row(stored.kind, stored.id, &bytes).unwrap();
        let mut incoming = sample();
        incoming.fee = Some(100);
        let merged = merge(old, incoming);
        assert_eq!(merged.block_hash, stored.block_hash);
        assert_eq!(merged.tx_index, stored.tx_index);
        assert_eq!(merged.log_index, stored.log_index);
        assert_eq!(merged.fee, Some(100));
        let again = encode_row(&merged).unwrap();
        assert_eq!(again[0], RECORD_VERSION_OBS);
        assert_eq!(decode_row(merged.kind, merged.id, &again).unwrap(), merged);
    }

    #[test]
    fn page_venues_excludes_the_after_id_and_other_kinds() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let mut rows = Vec::new();
        for n in 1..=3 {
            let mut row = sample();
            row.kind = UNI_V3;
            let mut id = B256::ZERO;
            id[31] = n;
            row.id = id;
            rows.push(row);
        }
        let mut other = rows[0].clone();
        other.kind = UNI_V2;
        catalog.commit_chunk("page", 1, &rows, Put::Merge).unwrap();
        catalog
            .commit_chunk("page", 2, &[other], Put::Merge)
            .unwrap();
        let page = catalog.page_venues(UNI_V3, None, 2).unwrap();
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id[31], 1);
        let next = catalog.page_venues(UNI_V3, Some(page[1].id), 2).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id[31], 3);
        assert!(catalog
            .page_venues(UNI_V3, Some(next[0].id), 2)
            .unwrap()
            .is_empty());
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
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let row = sample();
        catalog
            .commit_chunk("list:test", 10, std::slice::from_ref(&row), Put::Merge)
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
        assert!(dir.0.join("entities.redb").exists());
        assert!(dir.0.join("discovery.redb").exists());
        assert!(dir.0.join("work.redb").exists());
        assert!(!dir.0.join("venues.redb").exists());
    }

    #[test]
    fn hits_and_bodies_survive_a_reopen() {
        let dir = TempDir::new();
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
            let catalog = Catalog::open(&dir.0).unwrap();
            assert!(catalog.hit_scan("logs:test").unwrap().is_none());
            catalog.commit_hits("logs:test", 10, &[3, 4]).unwrap();
            catalog.commit_hits("logs:test", 8, &[1]).unwrap();
            catalog.put_bodies(&[(4, body.clone())]).unwrap();
        }
        let catalog = Catalog::open(&dir.0).unwrap();
        assert_eq!(catalog.hit_scan("logs:test").unwrap(), Some(10));
        assert_eq!(
            catalog.hits_from("logs:test", 0, 10).unwrap(),
            vec![1, 3, 4]
        );
        assert_eq!(catalog.hits_from("logs:test", 4, 5).unwrap(), vec![4]);
        let stored = catalog.read_bodies(&[4, 5]).unwrap();
        assert_eq!(stored[0].as_ref(), Some(&body));
        assert!(stored[1].is_none());
    }

    #[test]
    fn a_legacy_venues_file_is_renamed_and_crawl_tables_move() {
        let dir = TempDir::new();
        let legacy = dir.0.join("venues.redb");
        let db = Database::create(&legacy).unwrap();
        {
            let mut write = db.begin_write().unwrap();
            write.set_durability(Durability::Immediate);
            {
                let mut cursors = write.open_table(CURSORS).unwrap();
                cursors.insert("logs:old", 42_u64).unwrap();
                let mut venues = write.open_table(VENUES).unwrap();
                let row = sample();
                let bytes = encode_row(&row).unwrap();
                venues
                    .insert(row.key().as_slice(), bytes.as_slice())
                    .unwrap();
            }
            write.commit().unwrap();
        }
        drop(db);
        let catalog = Catalog::open(&legacy).unwrap();
        assert!(!legacy.exists());
        assert!(dir.0.join("entities.redb").exists());
        assert_eq!(catalog.cursor("logs:old").unwrap(), Some(42));
        let page = catalog.page_venues(sample().kind, None, 10).unwrap();
        assert_eq!(page, vec![sample()]);
        drop(catalog);
        let again = Catalog::open(&dir.0.join("entities.redb")).unwrap();
        assert_eq!(again.cursor("logs:old").unwrap(), Some(42));
        let untouched = dir.0.join("venues.redb");
        std::fs::write(&untouched, b"leave-me").unwrap();
        drop(again);
        let _ = Catalog::open(&dir.0).unwrap();
        assert_eq!(std::fs::read(&untouched).unwrap(), b"leave-me");
    }

    #[test]
    fn sample_takes_small_kinds_and_a_stride_of_the_large_one() {
        let src_dir = TempDir::new();
        let src = src_dir.0.join("venues.redb");
        let db = Database::create(&src).unwrap();
        {
            let write = db.begin_write().unwrap();
            {
                let mut venues = write.open_table(VENUES).unwrap();
                for index in 0..30_u8 {
                    let mut row = sample();
                    row.kind = if index < 5 { V3_FORK } else { UNI_V2 };
                    row.id[31] = index;
                    let bytes = encode_row(&row).unwrap();
                    venues
                        .insert(row.key().as_slice(), bytes.as_slice())
                        .unwrap();
                }
            }
            write.commit().unwrap();
        }
        drop(db);
        let dest = TempDir::new();
        let report = sample_identities(&src, &dest.0, 10).unwrap();
        assert_eq!(report.copied, 10);
        assert_eq!(report.by_kind.get(&V3_FORK).copied(), Some(5));
        assert_eq!(report.by_kind.get(&UNI_V2).copied(), Some(5));
        assert!(src.exists());
        assert!(!src_dir.0.join("entities.redb").exists());
        assert!(dest.0.join("entities.redb").exists());
        let catalog = Catalog::open(&dest.0).unwrap();
        assert_eq!(catalog.page_venues(V3_FORK, None, 10).unwrap().len(), 5);
    }

    #[test]
    fn fill_adds_identities_the_sample_did_not_copy() {
        let src_dir = TempDir::new();
        let src = src_dir.0.join("venues.redb");
        let db = Database::create(&src).unwrap();
        {
            let write = db.begin_write().unwrap();
            {
                let mut venues = write.open_table(VENUES).unwrap();
                for index in 0..30_u8 {
                    let mut row = sample();
                    row.kind = UNI_V2;
                    row.id[31] = index;
                    let bytes = encode_row(&row).unwrap();
                    venues
                        .insert(row.key().as_slice(), bytes.as_slice())
                        .unwrap();
                }
            }
            write.commit().unwrap();
        }
        drop(db);
        let dest = TempDir::new();
        let first = sample_identities(&src, &dest.0, 10).unwrap();
        assert_eq!(first.copied, 10);
        let filled = fill_identities(&src, &dest.0, 5).unwrap();
        assert_eq!(filled.copied, 5);
        assert_eq!(filled.ids.len(), 5);
        let catalog = Catalog::open(&dest.0).unwrap();
        assert_eq!(catalog.page_venues(UNI_V2, None, 30).unwrap().len(), 15);
        drop(catalog);
        let again = fill_identities(&src, &dest.0, 5).unwrap();
        assert_eq!(again.copied, 5);
        let catalog = Catalog::open(&dest.0).unwrap();
        assert_eq!(catalog.page_venues(UNI_V2, None, 30).unwrap().len(), 20);
        let mut seen = std::collections::HashSet::new();
        for row in catalog.page_venues(UNI_V2, None, 30).unwrap() {
            assert!(seen.insert(row.id));
        }
    }

    #[test]
    fn sample_keeps_the_anchor_pool_off_the_stride() {
        let src_dir = TempDir::new();
        let src = src_dir.0.join("venues.redb");
        let db = Database::create(&src).unwrap();
        {
            let write = db.begin_write().unwrap();
            {
                let mut venues = write.open_table(VENUES).unwrap();
                for index in 0..20_u8 {
                    let mut row = sample();
                    row.kind = UNI_V3;
                    row.id[31] = index;
                    if index == 19 {
                        row.id = anchor_id();
                    }
                    let bytes = encode_row(&row).unwrap();
                    venues
                        .insert(row.key().as_slice(), bytes.as_slice())
                        .unwrap();
                }
            }
            write.commit().unwrap();
        }
        drop(db);
        let dest = TempDir::new();
        let report = sample_identities(&src, &dest.0, 3).unwrap();
        assert!(report.anchor);
        assert!(report.copied >= 3);
        let catalog = Catalog::open(&dest.0).unwrap();
        let rows = catalog.page_venues(UNI_V3, None, 10).unwrap();
        assert!(rows.iter().any(|row| row.id == anchor_id()));
    }

    #[test]
    fn allocate_covers_every_live_kind_near_ten_thousand() {
        let counts = [
            (1_u8, 527_256_u64),
            (2, 4_818),
            (3, 8_110),
            (4, 212),
            (5, 48_693),
            (6, 1_498),
            (7, 253),
            (8, 12),
            (9, 102),
            (10, 192_676),
            (11, 334),
            (12, 253),
            (13, 2_501),
            (14, 190),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        let plan = allocate_sample(&counts, 10_000);
        let total: u64 = plan.values().sum();
        assert_eq!(total, 10_000);
        for kind in [4_u8, 6, 7, 8, 9, 11, 12, 13, 14] {
            assert_eq!(plan.get(&kind).copied(), counts.get(&kind).copied());
        }
        for kind in [1_u8, 2, 3, 5, 10] {
            assert!(plan.get(&kind).copied().unwrap_or(0) >= 400, "{plan:?}");
        }
    }
}
