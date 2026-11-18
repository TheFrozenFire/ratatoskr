//! Row counts for the state pipeline.

use alloy_primitives::B256;
use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata};
use store_model::{StateSummary, VenueRow, STAGE_CONDUIT};

use super::super::catalog::Catalog;
use super::super::catalog::VENUES;
use super::*;

impl Catalog {
    pub fn state_summary(&self) -> eyre::Result<StateSummary> {
        let mut summary = StateSummary {
            identities: count_bytes(&self.entities, VENUES)?,
            immutable: count_bytes(&self.entities, IMMUTABLE)?,
            mutable: count_bytes(&self.entities, MUTABLE)?,
            curves: count_bytes(&self.entities, CURVES)?,
            conduits: super::super::conduit::count_conduits(&self.entities)?,
            tick_pages: count_bytes(&self.entities, TICK_PAGES)?,
            mismatches: count_bytes(&self.entities, MISMATCH)?,
            tokens: self.token_count()?,
            ..StateSummary::default()
        };
        super::work::ensure_state_accounts(&self.work)?;
        let read = self.work.begin_read()?;
        match read.open_table(super::work::JOB_STAGE) {
            Ok(stages) => {
                for item in stages.iter()? {
                    let (key, value) = item?;
                    let key = key.value();
                    if key.len() != 1 || value.value() == 0 {
                        continue;
                    }
                    summary.jobs += value.value();
                    summary.jobs_by_stage.insert(key[0], value.value());
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
        match read.open_table(super::work::JOB_ERROR) {
            Ok(errors) => {
                for item in errors.iter()? {
                    let (_, value) = item?;
                    let error = String::from_utf8_lossy(value.value()).into_owned();
                    if !error.is_empty() {
                        *summary.errors.entry(error).or_default() += 1;
                    }
                }
            }
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(err) => return Err(err.into()),
        }
        drop(read);
        let (conduit_jobs, conduit_errors) = super::super::conduit::account_jobs(&self.work)?;
        summary.jobs += conduit_jobs;
        if conduit_jobs > 0 {
            summary.jobs_by_stage.insert(STAGE_CONDUIT, conduit_jobs);
        }
        for (error, count) in conduit_errors {
            *summary.errors.entry(error).or_default() += count;
        }
        Ok(summary)
    }

    pub fn read_venue(&self, kind: u8, id: B256) -> eyre::Result<Option<VenueRow>> {
        super::super::catalog::read_venue_row(&self.entities, kind, id)
    }
}

fn count_bytes(db: &redb::Database, def: TableDefinition<&[u8], &[u8]>) -> eyre::Result<u64> {
    let read = db.begin_read()?;
    let table = match read.open_table(def) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    Ok(table.len()?)
}
