//! Answer follow cursor.

use alloy_primitives::{Address, I256};
use redb::{Durability, ReadableDatabase, ReadableTable};
use store_model::ObservedAnswer;

use super::answer::{answer_key, encode_answer};
use super::phase::{decode_phase, split_phase_key};
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
