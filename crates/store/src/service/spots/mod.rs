//! `spots.redb`. One row is one token in one numeraire.
//!
//! A pass replaces the table, so a token that lost its route is gone. The
//! table is not the feed answer table, and this file is not `feeds.redb`.

mod pass;
mod row;

use crate::storage::Database;
use redb::TableDefinition;
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
