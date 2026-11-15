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

/// One pass replaces the table. The cache holds that write.
const PAGE_CACHE: usize = 8 * 1024 * 1024;

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
        let db = Database::open_or_create(path, PAGE_CACHE).map_err(db_err)?;
        db.watch();
        Ok(Self { db })
    }
}
