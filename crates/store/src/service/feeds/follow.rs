//! Answer follow cursor.

use redb::{Durability, ReadableDatabase};
use store_model::AnswerPut;

use super::answer::{answer_key, encode_answer};
use super::*;

impl Feeds {
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

    /// Advance the follow cursor and insert `rows`.
    ///
    /// The cursor does not move backward.
    pub fn commit_follow(&self, next_block: u64, rows: &[AnswerPut]) -> Result<usize, Error> {
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
            let mut answers = write.open_table(ANSWERS).map_err(db_err)?;
            for row in rows {
                let key = answer_key(row.proxy, row.phase_id, row.observed.round_id);
                let value = encode_answer(&row.observed);
                answers
                    .insert(key.as_slice(), value.as_slice())
                    .map_err(db_err)?;
            }
            let written = rows.len();
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
