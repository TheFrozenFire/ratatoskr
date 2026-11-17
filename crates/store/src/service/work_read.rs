//! Read-only pages of `work.redb`.

use alloy_primitives::{Address, B256};
use redb::{ReadableDatabase, TableDefinition};
use std::path::Path;
use store_model::{decode_conduit_job, decode_job, ConduitJob, WorkJob};

use crate::storage::ReadDatabase;

const JOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("jobs");
const CONDUIT_JOBS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_jobs");

const STAGES: [u8; 5] = [1, 2, 3, 4, 6];

/// `work.redb` while another process writes it.
pub struct WorkRead {
    db: ReadDatabase,
}

impl WorkRead {
    pub fn open(path: &Path, cache: usize) -> eyre::Result<Self> {
        Ok(Self {
            db: ReadDatabase::open(path, cache)?,
        })
    }

    /// Jobs in key order. `stage` limits the page to one stage.
    ///
    /// Rows that carry an error come first inside the page.
    pub fn page_jobs(
        &self,
        stage: Option<u8>,
        after: Option<&[u8]>,
        limit: usize,
    ) -> eyre::Result<Vec<WorkJob>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut page = Vec::new();
        let rows = match (stage, after) {
            (Some(stage), Some(after)) => {
                let end = stage_end(stage);
                table.range::<&[u8]>(after..end.as_slice())?
            }
            (Some(stage), None) => {
                let start = [stage];
                let end = stage_end(stage);
                table.range::<&[u8]>(start.as_slice()..end.as_slice())?
            }
            (None, Some(after)) => table.range::<&[u8]>(after..)?,
            (None, None) => table.range::<&[u8]>(..)?,
        };
        for item in rows {
            if page.len() == limit {
                break;
            }
            let (key, value) = item?;
            if after.is_some_and(|cursor| key.value() == cursor) {
                continue;
            }
            page.push(decode_job(key.value(), value.value())?);
        }
        page.sort_by_key(|job| job.error.is_empty());
        Ok(page)
    }

    /// The job for this pool, whichever stage holds it.
    pub fn job_for(&self, kind: u8, id: B256) -> eyre::Result<Option<WorkJob>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(err) => return Err(err.into()),
        };
        for stage in STAGES {
            let key = job_key(stage, kind, &id);
            if let Some(value) = table.get(key.as_slice())? {
                return Ok(Some(decode_job(&key, value.value())?));
            }
        }
        Ok(None)
    }

    pub fn count_stage(&self, stage: u8) -> eyre::Result<u64> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        let end = stage_end(stage);
        let mut count = 0_u64;
        for item in table.range::<&[u8]>([stage].as_slice()..end.as_slice())? {
            item?;
            count += 1;
        }
        Ok(count)
    }

    pub fn page_conduit_jobs(
        &self,
        after: Option<&[u8]>,
        limit: usize,
    ) -> eyre::Result<Vec<ConduitJob>> {
        let read = self.db.begin_read()?;
        let table = match read.open_table(CONDUIT_JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut page = Vec::new();
        let rows = match after {
            Some(after) => table.range::<&[u8]>(after..)?,
            None => table.range::<&[u8]>(..)?,
        };
        for item in rows {
            if page.len() == limit {
                break;
            }
            let (key, value) = item?;
            if after.is_some_and(|cursor| key.value() == cursor) {
                continue;
            }
            page.push(decode_conduit_job(key.value(), value.value())?);
        }
        page.sort_by_key(|job| job.error.is_empty());
        Ok(page)
    }
}

fn stage_end(stage: u8) -> [u8; 1] {
    [stage.saturating_add(1)]
}

fn job_key(stage: u8, kind: u8, id: &B256) -> [u8; 34] {
    let mut out = [0u8; 34];
    out[0] = stage;
    out[1] = kind;
    out[2..].copy_from_slice(id.as_slice());
    out
}

/// Address pair key used by conduit jobs. Exposed so a page can resume.
pub fn conduit_job_key(token_in: Address, token_out: Address) -> [u8; 40] {
    let mut out = [0u8; 40];
    out[..20].copy_from_slice(token_in.as_slice());
    out[20..].copy_from_slice(token_out.as_slice());
    out
}
