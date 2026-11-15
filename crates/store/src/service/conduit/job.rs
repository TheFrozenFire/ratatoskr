//! Conduit jobs in `work.redb`.

use alloy_primitives::Address;
use redb::{Durability, ReadableDatabase, ReadableTable};
use std::collections::{BTreeMap, BTreeSet};
use store_model::{decode_conduit_job, encode_conduit_job, ConduitFail, ConduitJob};

use super::super::catalog::Catalog;
use super::*;

impl Catalog {
    pub fn claim_conduits(
        &self,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<ConduitJob>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let jobs = {
            let mut table = write.open_table(JOBS)?;
            let mut picked = Vec::new();
            for item in table.iter()? {
                if picked.len() == limit as usize {
                    break;
                }
                let (key, value) = item?;
                let mut job = decode_job(key.value(), value.value())?;
                if job.lease_until > now || job.not_before > now {
                    continue;
                }
                job.lease_until = now.saturating_add(lease_for);
                job.attempt = job.attempt.saturating_add(1);
                job.rebuild = false;
                picked.push((key.value().to_vec(), job));
            }
            let mut jobs = Vec::with_capacity(picked.len());
            for (key, job) in picked {
                let bytes = encode_job(&job);
                table.insert(key.as_slice(), bytes.as_slice())?;
                jobs.push(job);
            }
            jobs
        };
        write.commit()?;
        Ok(jobs)
    }

    /// `again` keeps the job after a successful build whose members moved.
    pub fn finish_conduits(
        &self,
        done: &[(Address, Address)],
        again: &[(Address, Address)],
        failed: &[ConduitFail],
    ) -> eyre::Result<()> {
        let again_set: BTreeSet<(Address, Address)> = again.iter().copied().collect();
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut table = write.open_table(JOBS)?;
            for pair in done {
                let key = pair_key(pair.0, pair.1);
                let Some(stored) = table.get(key.as_slice())? else {
                    continue;
                };
                let mut job = decode_job(key.as_slice(), stored.value())?;
                drop(stored);
                if job.rebuild || again_set.contains(pair) {
                    job.rebuild = false;
                    job.lease_until = 0;
                    job.not_before = 0;
                    job.error.clear();
                    let bytes = encode_job(&job);
                    table.insert(key.as_slice(), bytes.as_slice())?;
                } else {
                    table.remove(key.as_slice())?;
                }
            }
            for fail in failed {
                let key = pair_key(fail.token_in, fail.token_out);
                let Some(stored) = table.get(key.as_slice())? else {
                    continue;
                };
                let mut job = decode_job(key.as_slice(), stored.value())?;
                drop(stored);
                job.lease_until = 0;
                job.not_before = fail.not_before;
                job.error = clip_error(&fail.error);
                if job.rebuild {
                    job.rebuild = false;
                    job.not_before = 0;
                }
                let bytes = encode_job(&job);
                table.insert(key.as_slice(), bytes.as_slice())?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// Build every conduit job that is ready. A job that fails stays for a later pass.
    pub fn drain_conduits(&self) -> eyre::Result<u64> {
        let mut built = 0_u64;
        loop {
            let now = super::super::state::unix_now();
            let jobs = self.claim_conduits(32, now, 120)?;
            if jobs.is_empty() {
                return Ok(built);
            }
            let mut done = Vec::new();
            let mut again = Vec::new();
            let mut failed = Vec::new();
            for job in &jobs {
                match self.build_conduit(job.token_in, job.token_out) {
                    Ok(true) => {
                        again.push((job.token_in, job.token_out));
                        done.push((job.token_in, job.token_out));
                    }
                    Ok(false) => done.push((job.token_in, job.token_out)),
                    Err(err) => failed.push(ConduitFail {
                        token_in: job.token_in,
                        token_out: job.token_out,
                        not_before: now.saturating_add(60),
                        error: clip_error(&err.to_string()),
                    }),
                }
            }
            self.finish_conduits(&done, &again, &failed)?;
            built += done.len() as u64;
            if built.is_multiple_of(256) {
                observability::info!(built, "conduit drain");
            }
        }
    }
}

pub(crate) fn account_jobs(db: &redb::Database) -> eyre::Result<(u64, BTreeMap<String, u64>)> {
    let mut errors = BTreeMap::new();
    let read = db.begin_read()?;
    let table = match read.open_table(JOBS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok((0, errors)),
        Err(err) => return Err(err.into()),
    };
    let mut jobs = 0_u64;
    for item in table.iter()? {
        let (key, value) = item?;
        let job = decode_job(key.value(), value.value())?;
        jobs += 1;
        if !job.error.is_empty() {
            *errors.entry(job.error).or_default() += 1;
        }
    }
    Ok((jobs, errors))
}

fn encode_job(job: &ConduitJob) -> Vec<u8> {
    encode_conduit_job(job)
}

fn decode_job(key: &[u8], bytes: &[u8]) -> eyre::Result<ConduitJob> {
    decode_conduit_job(key, bytes)
}

pub(super) fn sync_jobs(
    catalog: &Catalog,
    alive: &[(Address, Address)],
    gone: &[(Address, Address)],
    now: u64,
) -> eyre::Result<()> {
    if alive.is_empty() && gone.is_empty() {
        return Ok(());
    }
    let mut write = catalog.work.begin_write()?;
    write.set_durability(Durability::Immediate)?;
    {
        let mut table = write.open_table(JOBS)?;
        for pair in gone {
            let key = pair_key(pair.0, pair.1);
            let Some(stored) = table.get(key.as_slice())? else {
                continue;
            };
            let job = decode_job(key.as_slice(), stored.value())?;
            drop(stored);
            if job.lease_until > now {
                let mut job = job;
                job.rebuild = true;
                let bytes = encode_job(&job);
                table.insert(key.as_slice(), bytes.as_slice())?;
            } else {
                table.remove(key.as_slice())?;
            }
        }
        for pair in alive {
            let key = pair_key(pair.0, pair.1);
            let job = {
                let existing = table.get(key.as_slice())?;
                if let Some(stored) = existing {
                    let mut job = decode_job(key.as_slice(), stored.value())?;
                    drop(stored);
                    if job.lease_until > now {
                        job.rebuild = true;
                    } else {
                        job.not_before = 0;
                        job.error.clear();
                    }
                    job
                } else {
                    ConduitJob {
                        token_in: pair.0,
                        token_out: pair.1,
                        lease_until: 0,
                        attempt: 0,
                        not_before: 0,
                        rebuild: false,
                        error: String::new(),
                    }
                }
            };
            let bytes = encode_job(&job);
            table.insert(key.as_slice(), bytes.as_slice())?;
        }
    }
    write.commit()?;
    Ok(())
}

fn clip_error(error: &str) -> String {
    let mut end = error.len().min(180);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
}
