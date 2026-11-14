//! Feed identities, configuration, phases, and unclaimed history jobs.
//!
//! Directory facts rewrite on each apply. Decimals, description, and version
//! are written once. A new chain phase appends, and that write inserts one
//! history job. Deleting the job table leaves the feeds and the phases.

use crate::abi::ProxyHead;
use crate::directory::{parse_directory, DirectoryError, DirectoryRow};
use alloy_primitives::{Address, I256, U256};
use redb::{Durability, ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;

const IDENTITY: TableDefinition<&[u8], &[u8]> = TableDefinition::new("identity");
const CONFIG: TableDefinition<&[u8], &[u8]> = TableDefinition::new("config");
const PHASES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("phases");
const JOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("jobs");
const ANSWERS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("answers");
const MISMATCHES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("mismatches");
const FAILURES: TableDefinition<&[u8], &[u8]> = TableDefinition::new("failures");
const META: TableDefinition<&str, u64> = TableDefinition::new("meta");

const LISTING: &str = "listing";
const FOLLOW: &str = "follow";
const LOOKBACK: &str = "lookback";
const ANSWER_COUNT: &str = "answer_count";
const IDENTITY_VERSION: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Directory(#[from] DirectoryError),
    #[error("proxy {proxy} is not listed")]
    Unknown { proxy: Address },
    #[error("{0}")]
    Store(String),
}

fn db_err(err: impl std::fmt::Display) -> Error {
    Error::Store(err.to_string())
}

fn count_rows(
    read: &redb::ReadTransaction,
    def: TableDefinition<&[u8], &[u8]>,
) -> Result<u64, Error> {
    let table = match read.open_table(def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(err) => return Err(db_err(err)),
    };
    count_open(&table)
}

fn count_open(table: &impl ReadableTable<&'static [u8], &'static [u8]>) -> Result<u64, Error> {
    let mut count = 0_u64;
    for item in table.iter().map_err(db_err)? {
        item.map_err(db_err)?;
        count += 1;
    }
    Ok(count)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub generation: u64,
    pub named: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feed {
    pub row: DirectoryRow,
    pub listed_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configuration {
    pub decimals: u8,
    pub description: String,
    pub version: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase {
    pub phase_id: u16,
    pub aggregator: Address,
    pub min_answer: Option<I256>,
    pub max_answer: Option<I256>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryJob {
    pub proxy: Address,
    pub phase_id: u16,
    pub aggregator: Address,
}

/// Counts the status view reads for the feed file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gauges {
    pub feeds: u64,
    pub configured: u64,
    pub answers: u64,
    pub jobs: u64,
    pub follow: Option<u64>,
    pub lookback: bool,
}

/// One answer taken from an aggregator `AnswerUpdated` log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub proxy: Address,
    pub phase_id: u16,
    pub round_id: u64,
    pub value: I256,
    pub updated_at: u64,
    pub block: u64,
    pub log_index: u64,
}

/// A log the follow pass has already decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedAnswer {
    pub aggregator: Address,
    pub round_id: u64,
    pub value: I256,
    pub updated_at: u64,
    pub block: u64,
    pub log_index: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mismatch {
    Configuration {
        stored_decimals: u8,
        observed_decimals: u8,
        stored_description: String,
        observed_description: String,
        stored_version: U256,
        observed_version: U256,
    },
    Phase {
        newest: u16,
        observed: u16,
    },
    Aggregator {
        phase_id: u16,
        stored: Address,
        observed: Address,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    Failed(String),
    Configured {
        head: ProxyHead,
        min_answer: Option<I256>,
        max_answer: Option<I256>,
    },
}

pub struct Feeds {
    db: redb::Database,
}

impl Feeds {
    pub fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(db_err)?;
            }
        }
        let mut builder = redb::Builder::new();
        builder.set_cache_size(8 * 1024 * 1024);
        builder.set_concurrency_mode(redb::ConcurrencyMode::SingleWriter);
        let db = if path.exists() {
            builder.open(path)
        } else {
            builder.create(path)
        }
        .map_err(db_err)?;
        Ok(Self { db })
    }

    pub fn apply_directory(&self, body: &str) -> Result<Listing, Error> {
        let rows = parse_directory(body)?;
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        let generation = {
            let mut meta = write.open_table(META).map_err(db_err)?;
            let generation = meta
                .get(LISTING)
                .map_err(db_err)?
                .map(|value| value.value())
                .unwrap_or(0)
                + 1;
            meta.insert(LISTING, generation).map_err(db_err)?;
            generation
        };
        {
            let mut identity = write.open_table(IDENTITY).map_err(db_err)?;
            for row in &rows {
                let value = encode_identity(row, generation)?;
                identity
                    .insert(row.proxy.as_slice(), value.as_slice())
                    .map_err(db_err)?;
            }
        }
        write.commit().map_err(db_err)?;
        Ok(Listing {
            generation,
            named: rows.len(),
        })
    }

    pub fn listing_generation(&self) -> Result<u64, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(db_err(err)),
        };
        Ok(table
            .get(LISTING)
            .map_err(db_err)?
            .map(|value| value.value())
            .unwrap_or(0))
    }

    pub fn feed(&self, proxy: Address) -> Result<Option<Feed>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(IDENTITY) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(db_err(err)),
        };
        let Some(value) = table.get(proxy.as_slice()).map_err(db_err)? else {
            return Ok(None);
        };
        Ok(Some(decode_identity(proxy, value.value())?))
    }

    pub fn proxies(&self) -> Result<Vec<Address>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(IDENTITY) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut out = Vec::new();
        for item in table.iter().map_err(db_err)? {
            let (key, _) = item.map_err(db_err)?;
            let bytes = key.value();
            if bytes.len() != 20 {
                return Err(Error::Store("proxy key is short".into()));
            }
            out.push(Address::from_slice(bytes));
        }
        Ok(out)
    }

    pub fn configuration(&self, proxy: Address) -> Result<Option<Configuration>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(CONFIG) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(db_err(err)),
        };
        let Some(value) = table.get(proxy.as_slice()).map_err(db_err)? else {
            return Ok(None);
        };
        Ok(Some(decode_configuration(value.value())?))
    }

    pub fn phases(&self, proxy: Address) -> Result<Vec<Phase>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(PHASES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let phases = read_phases(phase_rows(&table, proxy)?)?;
        Ok(phases)
    }

    pub fn history_jobs(&self) -> Result<Vec<HistoryJob>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut out = Vec::new();
        for item in table.iter().map_err(db_err)? {
            let (key, value) = item.map_err(db_err)?;
            let (proxy, phase_id) = split_phase_key(key.value())?;
            let aggregator = address_of(value.value())?;
            out.push(HistoryJob {
                proxy,
                phase_id,
                aggregator,
            });
        }
        Ok(out)
    }

    pub fn mismatches(&self, proxy: Address) -> Result<Vec<Mismatch>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(MISMATCHES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let start = mismatch_key(proxy, 0);
        let end = mismatch_key(proxy, u32::MAX);
        let mut out = Vec::new();
        for item in table
            .range(start.as_slice()..=end.as_slice())
            .map_err(db_err)?
        {
            let (_, value) = item.map_err(db_err)?;
            out.push(decode_mismatch(value.value())?);
        }
        Ok(out)
    }

    pub fn failure(&self, proxy: Address) -> Result<Option<String>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(FAILURES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(db_err(err)),
        };
        let Some(value) = table.get(proxy.as_slice()).map_err(db_err)? else {
            return Ok(None);
        };
        let text = std::str::from_utf8(value.value())
            .map_err(|_| Error::Store("failure is not utf-8".into()))?;
        Ok(Some(text.to_string()))
    }

    pub fn apply_read(&self, proxy: Address, read: Read) -> Result<(), Error> {
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        {
            let identity = write.open_table(IDENTITY).map_err(db_err)?;
            if identity.get(proxy.as_slice()).map_err(db_err)?.is_none() {
                return Err(Error::Unknown { proxy });
            }
            let mut failures = write.open_table(FAILURES).map_err(db_err)?;
            match read {
                Read::Failed(text) => {
                    failures
                        .insert(proxy.as_slice(), text.as_bytes())
                        .map_err(db_err)?;
                }
                Read::Configured {
                    head,
                    min_answer,
                    max_answer,
                } => {
                    failures.remove(proxy.as_slice()).map_err(db_err)?;
                    drop(failures);
                    drop(identity);
                    let mut config = write.open_table(CONFIG).map_err(db_err)?;
                    let mut mismatches = write.open_table(MISMATCHES).map_err(db_err)?;
                    let existing = match config.get(proxy.as_slice()).map_err(db_err)? {
                        None => None,
                        Some(stored) => Some(decode_configuration(stored.value())?),
                    };
                    match existing {
                        None => {
                            let value = encode_configuration(&Configuration {
                                decimals: head.decimals,
                                description: head.description.clone(),
                                version: head.version,
                            })?;
                            config
                                .insert(proxy.as_slice(), value.as_slice())
                                .map_err(db_err)?;
                        }
                        Some(stored)
                            if stored.decimals != head.decimals
                                || stored.description != head.description
                                || stored.version != head.version =>
                        {
                            let mismatch = Mismatch::Configuration {
                                stored_decimals: stored.decimals,
                                observed_decimals: head.decimals,
                                stored_description: stored.description,
                                observed_description: head.description.clone(),
                                stored_version: stored.version,
                                observed_version: head.version,
                            };
                            insert_mismatch(&mut mismatches, proxy, &mismatch)?;
                        }
                        Some(_) => {}
                    }
                    drop(config);
                    let mut phases = write.open_table(PHASES).map_err(db_err)?;
                    let mut jobs = write.open_table(JOBS).map_err(db_err)?;
                    let stored = read_phases(phase_rows(&phases, proxy)?)?;
                    match stored.last() {
                        Some(phase)
                            if phase.phase_id == head.phase_id
                                && phase.aggregator == head.aggregator => {}
                        Some(phase) if phase.phase_id == head.phase_id => {
                            let mismatch = Mismatch::Aggregator {
                                phase_id: head.phase_id,
                                stored: phase.aggregator,
                                observed: head.aggregator,
                            };
                            insert_mismatch(&mut mismatches, proxy, &mismatch)?;
                        }
                        Some(phase) if head.phase_id > phase.phase_id => {
                            insert_phase(
                                &mut phases,
                                &mut jobs,
                                proxy,
                                &head,
                                min_answer,
                                max_answer,
                            )?;
                        }
                        Some(phase) => {
                            let mismatch = Mismatch::Phase {
                                newest: phase.phase_id,
                                observed: head.phase_id,
                            };
                            insert_mismatch(&mut mismatches, proxy, &mismatch)?;
                        }
                        None => {
                            insert_phase(
                                &mut phases,
                                &mut jobs,
                                proxy,
                                &head,
                                min_answer,
                                max_answer,
                            )?;
                        }
                    }
                }
            }
        }
        write.commit().map_err(db_err)?;
        Ok(())
    }

    pub fn answers(&self, proxy: Address) -> Result<Vec<Answer>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(ANSWERS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let start = answer_key(proxy, 0, 0);
        let end = answer_key(proxy, u16::MAX, u64::MAX);
        let mut out = Vec::new();
        for item in table
            .range(start.as_slice()..=end.as_slice())
            .map_err(db_err)?
        {
            let (key, value) = item.map_err(db_err)?;
            out.push(decode_answer(key.value(), value.value())?);
        }
        Ok(out)
    }

    /// Feeds, stored configuration, answers, and unclaimed history jobs.
    pub fn gauges(&self) -> Result<Gauges, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let feeds = count_rows(&read, IDENTITY)?;
        let configured = count_rows(&read, CONFIG)?;
        let jobs = count_rows(&read, JOBS)?;
        let (answers, follow, lookback) = match read.open_table(META) {
            Ok(meta) => {
                let answers = meta
                    .get(ANSWER_COUNT)
                    .map_err(db_err)?
                    .map(|value| value.value())
                    .unwrap_or(0);
                let follow = meta.get(FOLLOW).map_err(db_err)?.map(|value| value.value());
                let lookback = meta.get(LOOKBACK).map_err(db_err)?.is_some();
                (answers, follow, lookback)
            }
            Err(redb::TableError::TableDoesNotExist(_)) => (0, None, false),
            Err(err) => return Err(db_err(err)),
        };
        Ok(Gauges {
            feeds,
            configured,
            answers,
            jobs,
            follow,
            lookback,
        })
    }

    /// Count stored answers once so later commits can add to that total.
    pub fn remember_answers(&self) -> Result<u64, Error> {
        let count = {
            let read = self.db.begin_read().map_err(db_err)?;
            if let Ok(meta) = read.open_table(META) {
                if let Some(count) = meta.get(ANSWER_COUNT).map_err(db_err)? {
                    return Ok(count.value());
                }
            }
            count_rows(&read, ANSWERS)?
        };
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        {
            let mut meta = write.open_table(META).map_err(db_err)?;
            if meta.get(ANSWER_COUNT).map_err(db_err)?.is_none() {
                meta.insert(ANSWER_COUNT, count).map_err(db_err)?;
            }
        }
        write.commit().map_err(db_err)?;
        Ok(count)
    }

    /// Next block the answer follow has not scanned. `None` means it has not started.
    pub fn follow_block(&self) -> Result<Option<u64>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(db_err(err)),
        };
        Ok(table
            .get(FOLLOW)
            .map_err(db_err)?
            .map(|value| value.value()))
    }

    pub fn lookback_done(&self) -> Result<bool, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(META) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
            Err(err) => return Err(db_err(err)),
        };
        Ok(table.get(LOOKBACK).map_err(db_err)?.is_some())
    }

    pub fn mark_lookback(&self) -> Result<(), Error> {
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        {
            let mut meta = write.open_table(META).map_err(db_err)?;
            meta.insert(LOOKBACK, 1u64).map_err(db_err)?;
        }
        write.commit().map_err(db_err)?;
        Ok(())
    }

    /// Store answers for phases whose aggregator emitted them, and advance the follow cursor.
    ///
    /// A zero value is not an answer. A repeated round stays as the first row.
    /// The cursor does not move backward.
    pub fn commit_follow(
        &self,
        next_block: u64,
        observed: &[ObservedAnswer],
    ) -> Result<usize, Error> {
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        let written = {
            let mut meta = write.open_table(META).map_err(db_err)?;
            if let Some(current) = meta.get(FOLLOW).map_err(db_err)?.map(|value| value.value()) {
                if next_block < current {
                    return Err(Error::Store(format!(
                        "follow cursor is {current}; {next_block} is below it"
                    )));
                }
            }
            meta.insert(FOLLOW, next_block).map_err(db_err)?;
            drop(meta);
            let phases = write.open_table(PHASES).map_err(db_err)?;
            let owners = phase_owners(&phases)?;
            drop(phases);
            let mut answers = write.open_table(ANSWERS).map_err(db_err)?;
            let mut written = 0usize;
            for observed in observed {
                if observed.value == I256::ZERO {
                    continue;
                }
                let Some(pairs) = owners.get(&observed.aggregator) else {
                    continue;
                };
                for (proxy, phase_id) in pairs {
                    let key = answer_key(*proxy, *phase_id, observed.round_id);
                    if answers.get(key.as_slice()).map_err(db_err)?.is_some() {
                        continue;
                    }
                    let value = encode_answer(observed);
                    answers
                        .insert(key.as_slice(), value.as_slice())
                        .map_err(db_err)?;
                    written += 1;
                }
            }
            drop(answers);
            let added = written as u64;
            let mut meta = write.open_table(META).map_err(db_err)?;
            let known = meta
                .get(ANSWER_COUNT)
                .map_err(db_err)?
                .map(|value| value.value());
            match known {
                Some(count) => {
                    meta.insert(ANSWER_COUNT, count.saturating_add(added))
                        .map_err(db_err)?;
                }
                None => {
                    drop(meta);
                    let answers = write.open_table(ANSWERS).map_err(db_err)?;
                    let total = count_open(&answers)?;
                    drop(answers);
                    let mut meta = write.open_table(META).map_err(db_err)?;
                    meta.insert(ANSWER_COUNT, total).map_err(db_err)?;
                }
            }
            written
        };
        write.commit().map_err(db_err)?;
        Ok(written)
    }

    /// Drop history jobs. Feeds, configuration, phases, and answers stay.
    pub fn clear_history_jobs(&self) -> Result<(), Error> {
        let mut write = self.db.begin_write().map_err(db_err)?;
        write
            .set_durability(Durability::Immediate)
            .map_err(db_err)?;
        {
            let mut jobs = write.open_table(JOBS).map_err(db_err)?;
            let mut keys = Vec::new();
            for item in jobs.iter().map_err(db_err)? {
                let (key, _) = item.map_err(db_err)?;
                keys.push(key.value().to_vec());
            }
            for key in keys {
                jobs.remove(key.as_slice()).map_err(db_err)?;
            }
        }
        write.commit().map_err(db_err)?;
        Ok(())
    }
}

fn insert_phase(
    phases: &mut redb::Table<&[u8], &[u8]>,
    jobs: &mut redb::Table<&[u8], &[u8]>,
    proxy: Address,
    head: &ProxyHead,
    min_answer: Option<I256>,
    max_answer: Option<I256>,
) -> Result<(), Error> {
    let key = phase_key(proxy, head.phase_id);
    let value = encode_phase(head.aggregator, min_answer, max_answer);
    phases
        .insert(key.as_slice(), value.as_slice())
        .map_err(db_err)?;
    jobs.insert(key.as_slice(), head.aggregator.as_slice())
        .map_err(db_err)?;
    Ok(())
}

fn insert_mismatch(
    table: &mut redb::Table<&[u8], &[u8]>,
    proxy: Address,
    mismatch: &Mismatch,
) -> Result<(), Error> {
    let start = mismatch_key(proxy, 0);
    let end = mismatch_key(proxy, u32::MAX);
    let mut seq = 0u32;
    for item in table
        .range(start.as_slice()..=end.as_slice())
        .map_err(db_err)?
    {
        let (key, _) = item.map_err(db_err)?;
        let (_, stored) = split_mismatch_key(key.value())?;
        seq = seq.max(stored.saturating_add(1));
    }
    let value = encode_mismatch(mismatch)?;
    table
        .insert(mismatch_key(proxy, seq).as_slice(), value.as_slice())
        .map_err(db_err)?;
    Ok(())
}

fn read_phases<'g, I>(rows: I) -> Result<Vec<Phase>, Error>
where
    I: Iterator<
        Item = Result<
            (
                redb::AccessGuard<'g, &'static [u8]>,
                redb::AccessGuard<'g, &'static [u8]>,
            ),
            redb::StorageError,
        >,
    >,
{
    let mut out = Vec::new();
    for item in rows {
        let (key, value) = item.map_err(db_err)?;
        let (_, phase_id) = split_phase_key(key.value())?;
        let (aggregator, min_answer, max_answer) = decode_phase(value.value())?;
        out.push(Phase {
            phase_id,
            aggregator,
            min_answer,
            max_answer,
        });
    }
    Ok(out)
}

fn phase_rows<'a, T>(
    table: &'a T,
    proxy: Address,
) -> Result<
    impl Iterator<
            Item = Result<
                (
                    redb::AccessGuard<'a, &'static [u8]>,
                    redb::AccessGuard<'a, &'static [u8]>,
                ),
                redb::StorageError,
            >,
        > + 'a,
    Error,
>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
{
    let start = phase_key(proxy, 0);
    let end = phase_key(proxy, u16::MAX);
    table
        .range(start.as_slice()..=end.as_slice())
        .map_err(db_err)
}

fn encode_identity(row: &DirectoryRow, listed_generation: u64) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    out.push(IDENTITY_VERSION);
    for text in [
        row.name.as_str(),
        row.path.as_str(),
        row.category.as_str(),
        row.feed_type.as_str(),
        row.multiply.as_str(),
        row.threshold.as_str(),
        row.base_asset.as_str(),
        row.quote_asset.as_str(),
        row.product_type.as_str(),
        row.product_type_code.as_str(),
        row.attribute_type.as_str(),
        row.market_hours.as_str(),
        row.shutdown_date.as_str(),
        row.clic_product_name.as_str(),
        row.svr_label.as_str(),
    ] {
        put_str(&mut out, text)?;
    }
    out.extend_from_slice(&row.contract_version.to_le_bytes());
    out.push(row.decimals);
    out.extend_from_slice(&row.heartbeat.to_le_bytes());
    out.push(u8::from(row.hidden));
    match row.secondary_proxy {
        None => out.push(0),
        Some(proxy) => {
            out.push(1);
            out.extend_from_slice(proxy.as_slice());
        }
    }
    out.extend_from_slice(row.directory_aggregator.as_slice());
    out.extend_from_slice(&listed_generation.to_le_bytes());
    Ok(out)
}

fn decode_identity(proxy: Address, bytes: &[u8]) -> Result<Feed, Error> {
    let mut cursor = Cursor { bytes, at: 0 };
    let version = cursor.u8()?;
    if version != IDENTITY_VERSION {
        return Err(Error::Store("identity version".into()));
    }
    let name = cursor.string()?;
    let path = cursor.string()?;
    let category = cursor.string()?;
    let feed_type = cursor.string()?;
    let multiply = cursor.string()?;
    let threshold = cursor.string()?;
    let base_asset = cursor.string()?;
    let quote_asset = cursor.string()?;
    let product_type = cursor.string()?;
    let product_type_code = cursor.string()?;
    let attribute_type = cursor.string()?;
    let market_hours = cursor.string()?;
    let shutdown_date = cursor.string()?;
    let clic_product_name = cursor.string()?;
    let svr_label = cursor.string()?;
    let contract_version = cursor.u64()?;
    let decimals = cursor.u8()?;
    let heartbeat = cursor.u64()?;
    let hidden = cursor.u8()? == 1;
    let secondary_proxy = match cursor.u8()? {
        0 => None,
        1 => Some(cursor.address()?),
        _ => return Err(Error::Store("secondary proxy".into())),
    };
    let directory_aggregator = cursor.address()?;
    let listed_generation = cursor.u64()?;
    Ok(Feed {
        row: DirectoryRow {
            proxy,
            name,
            path,
            category,
            feed_type,
            contract_version,
            decimals,
            multiply,
            heartbeat,
            threshold,
            base_asset,
            quote_asset,
            product_type,
            product_type_code,
            attribute_type,
            market_hours,
            shutdown_date,
            clic_product_name,
            hidden,
            secondary_proxy,
            svr_label,
            directory_aggregator,
        },
        listed_generation,
    })
}

fn encode_configuration(config: &Configuration) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    out.push(config.decimals);
    put_str(&mut out, &config.description)?;
    out.extend_from_slice(&config.version.to_be_bytes::<32>());
    Ok(out)
}

fn decode_configuration(bytes: &[u8]) -> Result<Configuration, Error> {
    let mut cursor = Cursor { bytes, at: 0 };
    let decimals = cursor.u8()?;
    let description = cursor.string()?;
    let version = cursor.word()?;
    Ok(Configuration {
        decimals,
        description,
        version,
    })
}

fn encode_phase(
    aggregator: Address,
    min_answer: Option<I256>,
    max_answer: Option<I256>,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(21 + 64);
    out.extend_from_slice(aggregator.as_slice());
    let mut flags = 0u8;
    if min_answer.is_some() {
        flags |= 1;
    }
    if max_answer.is_some() {
        flags |= 2;
    }
    out.push(flags);
    if let Some(min_answer) = min_answer {
        out.extend_from_slice(&min_answer.to_be_bytes::<32>());
    }
    if let Some(max_answer) = max_answer {
        out.extend_from_slice(&max_answer.to_be_bytes::<32>());
    }
    out
}

fn decode_phase(bytes: &[u8]) -> Result<(Address, Option<I256>, Option<I256>), Error> {
    if bytes.len() < 21 {
        return Err(Error::Store("phase is short".into()));
    }
    let aggregator = Address::from_slice(&bytes[..20]);
    let flags = bytes[20];
    let mut at = 21;
    let min_answer = if flags & 1 == 1 {
        Some(take_i256(bytes, &mut at)?)
    } else {
        None
    };
    let max_answer = if flags & 2 == 2 {
        Some(take_i256(bytes, &mut at)?)
    } else {
        None
    };
    Ok((aggregator, min_answer, max_answer))
}

fn encode_mismatch(mismatch: &Mismatch) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    match mismatch {
        Mismatch::Configuration {
            stored_decimals,
            observed_decimals,
            stored_description,
            observed_description,
            stored_version,
            observed_version,
        } => {
            out.push(1);
            out.push(*stored_decimals);
            out.push(*observed_decimals);
            put_str(&mut out, stored_description)?;
            put_str(&mut out, observed_description)?;
            out.extend_from_slice(&stored_version.to_be_bytes::<32>());
            out.extend_from_slice(&observed_version.to_be_bytes::<32>());
        }
        Mismatch::Phase { newest, observed } => {
            out.push(2);
            out.extend_from_slice(&newest.to_be_bytes());
            out.extend_from_slice(&observed.to_be_bytes());
        }
        Mismatch::Aggregator {
            phase_id,
            stored,
            observed,
        } => {
            out.push(3);
            out.extend_from_slice(&phase_id.to_be_bytes());
            out.extend_from_slice(stored.as_slice());
            out.extend_from_slice(observed.as_slice());
        }
    }
    Ok(out)
}

fn decode_mismatch(bytes: &[u8]) -> Result<Mismatch, Error> {
    let mut cursor = Cursor { bytes, at: 0 };
    match cursor.u8()? {
        1 => {
            let stored_decimals = cursor.u8()?;
            let observed_decimals = cursor.u8()?;
            let stored_description = cursor.string()?;
            let observed_description = cursor.string()?;
            let stored_version = cursor.word()?;
            let observed_version = cursor.word()?;
            Ok(Mismatch::Configuration {
                stored_decimals,
                observed_decimals,
                stored_description,
                observed_description,
                stored_version,
                observed_version,
            })
        }
        2 => Ok(Mismatch::Phase {
            newest: cursor.u16()?,
            observed: cursor.u16()?,
        }),
        3 => Ok(Mismatch::Aggregator {
            phase_id: cursor.u16()?,
            stored: cursor.address()?,
            observed: cursor.address()?,
        }),
        _ => Err(Error::Store("mismatch tag".into())),
    }
}

fn take_i256(bytes: &[u8], at: &mut usize) -> Result<I256, Error> {
    let end = at
        .checked_add(32)
        .ok_or_else(|| Error::Store("phase bound".into()))?;
    if end > bytes.len() {
        return Err(Error::Store("phase bound".into()));
    }
    let mut word = [0u8; 32];
    word.copy_from_slice(&bytes[*at..end]);
    *at = end;
    Ok(I256::from_be_bytes(word))
}

fn address_of(bytes: &[u8]) -> Result<Address, Error> {
    if bytes.len() != 20 {
        return Err(Error::Store("aggregator is short".into()));
    }
    Ok(Address::from_slice(bytes))
}

fn phase_owners(
    table: &impl ReadableTable<&'static [u8], &'static [u8]>,
) -> Result<std::collections::HashMap<Address, Vec<(Address, u16)>>, Error> {
    let mut owners = std::collections::HashMap::<Address, Vec<(Address, u16)>>::new();
    for item in table.iter().map_err(db_err)? {
        let (key, value) = item.map_err(db_err)?;
        let (proxy, phase_id) = split_phase_key(key.value())?;
        let (aggregator, _, _) = decode_phase(value.value())?;
        owners
            .entry(aggregator)
            .or_default()
            .push((proxy, phase_id));
    }
    Ok(owners)
}

fn answer_key(proxy: Address, phase_id: u16, round_id: u64) -> [u8; 30] {
    let mut key = [0u8; 30];
    key[..20].copy_from_slice(proxy.as_slice());
    key[20..22].copy_from_slice(&phase_id.to_be_bytes());
    key[22..].copy_from_slice(&round_id.to_be_bytes());
    key
}

fn encode_answer(observed: &ObservedAnswer) -> [u8; 56] {
    let mut out = [0u8; 56];
    out[..32].copy_from_slice(&observed.value.to_be_bytes::<32>());
    out[32..40].copy_from_slice(&observed.updated_at.to_le_bytes());
    out[40..48].copy_from_slice(&observed.block.to_le_bytes());
    out[48..].copy_from_slice(&observed.log_index.to_le_bytes());
    out
}

fn decode_answer(key: &[u8], value: &[u8]) -> Result<Answer, Error> {
    if key.len() != 30 || value.len() != 56 {
        return Err(Error::Store("answer row".into()));
    }
    let mut round = [0u8; 8];
    round.copy_from_slice(&key[22..30]);
    let mut word = [0u8; 32];
    word.copy_from_slice(&value[..32]);
    let mut updated = [0u8; 8];
    updated.copy_from_slice(&value[32..40]);
    let mut block = [0u8; 8];
    block.copy_from_slice(&value[40..48]);
    let mut log_index = [0u8; 8];
    log_index.copy_from_slice(&value[48..56]);
    Ok(Answer {
        proxy: Address::from_slice(&key[..20]),
        phase_id: u16::from_be_bytes([key[20], key[21]]),
        round_id: u64::from_be_bytes(round),
        value: I256::from_be_bytes(word),
        updated_at: u64::from_le_bytes(updated),
        block: u64::from_le_bytes(block),
        log_index: u64::from_le_bytes(log_index),
    })
}

fn phase_key(proxy: Address, phase_id: u16) -> [u8; 22] {
    let mut key = [0u8; 22];
    key[..20].copy_from_slice(proxy.as_slice());
    key[20..].copy_from_slice(&phase_id.to_be_bytes());
    key
}

fn split_phase_key(bytes: &[u8]) -> Result<(Address, u16), Error> {
    if bytes.len() != 22 {
        return Err(Error::Store("phase key".into()));
    }
    let phase_id = u16::from_be_bytes([bytes[20], bytes[21]]);
    Ok((Address::from_slice(&bytes[..20]), phase_id))
}

fn mismatch_key(proxy: Address, seq: u32) -> [u8; 24] {
    let mut key = [0u8; 24];
    key[..20].copy_from_slice(proxy.as_slice());
    key[20..].copy_from_slice(&seq.to_be_bytes());
    key
}

fn split_mismatch_key(bytes: &[u8]) -> Result<(Address, u32), Error> {
    if bytes.len() != 24 {
        return Err(Error::Store("mismatch key".into()));
    }
    let seq = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Ok((Address::from_slice(&bytes[..20]), seq))
}

fn put_str(out: &mut Vec<u8>, text: &str) -> Result<(), Error> {
    let len = u32::try_from(text.len()).map_err(|_| Error::Store("field is too long".into()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    Ok(())
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, len: usize) -> Result<&[u8], Error> {
        let end = self
            .at
            .checked_add(len)
            .ok_or_else(|| Error::Store("row is short".into()))?;
        if end > self.bytes.len() {
            return Err(Error::Store("row is short".into()));
        }
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Error> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        let bytes = self.take(8)?;
        let mut word = [0u8; 8];
        word.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(word))
    }

    fn word(&mut self) -> Result<U256, Error> {
        Ok(U256::from_be_slice(self.take(32)?))
    }

    fn address(&mut self) -> Result<Address, Error> {
        Ok(Address::from_slice(self.take(20)?))
    }

    fn string(&mut self) -> Result<String, Error> {
        let raw = self.take(4)?;
        let mut len_bytes = [0u8; 4];
        len_bytes.copy_from_slice(raw);
        let len = u32::from_le_bytes(len_bytes) as usize;
        let bytes = self.take(len)?;
        let text =
            std::str::from_utf8(bytes).map_err(|_| Error::Store("field is not utf-8".into()))?;
        Ok(text.to_string())
    }
}
