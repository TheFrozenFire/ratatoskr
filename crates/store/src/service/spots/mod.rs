//! `spots.redb`. One row is one token in one numeraire.
//!
//! A pass replaces the table, so a token that lost its route is gone. The
//! table is not the feed answer table, and this file is not `feeds.redb`.

mod pass;
mod row;

use crate::storage::Database;
use redb::{ReadableDatabase, ReadableTableMetadata, TableDefinition};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Store(String),
}

const SPOTS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("spots");
const VERSION: u8 = 1;

fn db_err(err: impl std::fmt::Display) -> Error {
    Error::Store(err.to_string())
}

pub struct Spots {
    db: Database,
}

/// `spots.redb` while another process writes it.
pub struct SpotRead {
    db: crate::storage::ReadDatabase,
}

impl SpotRead {
    pub fn open(path: &Path, cache: usize) -> Result<Self, Error> {
        Ok(Self {
            db: crate::storage::ReadDatabase::open(path, cache).map_err(db_err)?,
        })
    }

    pub fn page(
        &self,
        after: Option<&[u8]>,
        limit: usize,
    ) -> Result<Vec<store_model::Spot>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(SPOTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut page = Vec::new();
        let rows = match after {
            Some(after) => table.range::<&[u8]>(after..).map_err(db_err)?,
            None => table.range::<&[u8]>(..).map_err(db_err)?,
        };
        for item in rows {
            if page.len() == limit {
                break;
            }
            let (key, value) = item.map_err(db_err)?;
            if after.is_some_and(|cursor| key.value() == cursor) {
                continue;
            }
            page.push(row::decode(key.value(), value.value())?);
        }
        Ok(page)
    }

    pub fn for_token(
        &self,
        token: alloy_primitives::Address,
    ) -> Result<Vec<store_model::Spot>, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(SPOTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(db_err(err)),
        };
        let mut page = Vec::new();
        for item in table.range::<&[u8]>(token.as_slice()..).map_err(db_err)? {
            let (key, value) = item.map_err(db_err)?;
            if !key.value().starts_with(token.as_slice()) {
                break;
            }
            page.push(row::decode(key.value(), value.value())?);
        }
        Ok(page)
    }

    pub fn count(&self) -> Result<u64, Error> {
        let read = self.db.begin_read().map_err(db_err)?;
        let table = match read.open_table(SPOTS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(db_err(err)),
        };
        table.len().map_err(db_err)
    }
}

impl Spots {
    pub fn default_path() -> std::path::PathBuf {
        crate::storage::data_file("spots.redb")
    }

    pub fn open(path: &Path) -> Result<Self, Error> {
        Self::open_with(path, config::Store::default().spots)
    }

    pub fn open_with(path: &Path, cache: usize) -> Result<Self, Error> {
        let db = Database::open_or_create(path, cache).map_err(db_err)?;
        db.watch();
        Ok(Self { db })
    }
}
