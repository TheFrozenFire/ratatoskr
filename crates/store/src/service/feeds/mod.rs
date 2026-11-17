//! `feeds.redb`. Directory rows, configuration, phases, history jobs,
//! mismatches, failures, and answers.
//!
//! Deleting the job table leaves the feeds and the phases.

mod answer;
mod directory;
mod follow;
mod phase;

use crate::storage::{Database, ReadDatabase};
use alloy_primitives::{Address, U256};
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use std::path::Path;
use store_model::Gauges;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("proxy {proxy} is not listed")]
    Unknown { proxy: Address },
    #[error("{0}")]
    Store(String),
}

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

pub struct Feeds {
    db: Database,
}

/// One directory row and the newest answer stored for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedLine {
    pub feed: store_model::Feed,
    pub latest: Option<store_model::Answer>,
    pub history: bool,
}

/// A feed and the phases and answers stored under it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedDetail {
    pub feed: store_model::Feed,
    pub phases: Vec<store_model::Phase>,
    pub answers: Vec<store_model::Answer>,
}

/// `feeds.redb` while another process writes it.
pub struct FeedRead {
    db: ReadDatabase,
}

impl FeedRead {
    pub fn open(path: &std::path::Path, cache: usize) -> Result<Self, Error> {
        Ok(Self {
            db: ReadDatabase::open(path, cache).map_err(db_err)?,
        })
    }

    pub fn count(&self) -> Result<u64, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        count_rows(&read, IDENTITY)
    }
}

impl Feeds {
    pub fn default_path() -> std::path::PathBuf {
        crate::storage::data_file("feeds.redb")
    }

    pub fn open(path: &Path) -> Result<Self, Error> {
        Self::open_with(path, config::Store::default().feeds)
    }

    pub fn open_with(path: &Path, cache: usize) -> Result<Self, Error> {
        let db = Database::open_or_create(path, cache).map_err(db_err)?;
        db.watch();
        Ok(Self { db })
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
