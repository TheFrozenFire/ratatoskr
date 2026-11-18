//! Conduit jobs in `work.redb`.

use alloy_primitives::Address;
use redb::{Durability, ReadableDatabase, ReadableTable, ReadableTableMetadata};
use std::collections::{BTreeMap, BTreeSet};
use store_model::{decode_conduit_job, encode_conduit_job, ConduitFail, ConduitJob};

use super::super::catalog::Catalog;
use super::*;

/// Due time, then the pair. A claim reads only keys whose due time is `<= now`.
const RANK: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_job_rank");

const ERRORS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("conduit_job_errors");

const WORK_META: TableDefinition<&str, u64> = TableDefinition::new("work_meta");

const ERRORS_READY: &str = "conduit_errors";

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
            let mut ranks = write.open_table(RANK)?;
            if ranks.len()? == 0 && table.len()? > 0 {
                for item in table.iter()? {
                    let (key, value) = item?;
                    let job = decode_job(key.value(), value.value())?;
                    ranks.insert(ready_key(ready_at(&job), key.value()).as_slice(), &[][..])?;
                }
            }
            let end = due_end(now);
            let mut picked = Vec::new();
            let mut stale = Vec::new();
            let mut moved = Vec::new();
            for item in ranks.range(..end.as_slice())? {
                if picked.len() == limit as usize {
                    break;
                }
                let (rank, _) = item?;
                let rank_bytes = rank.value();
                if rank_bytes.len() != 48 {
                    stale.push(rank_bytes.to_vec());
                    continue;
                }
                let pair = rank_bytes[8..].to_vec();
                let Some(stored) = table.get(pair.as_slice())? else {
                    stale.push(rank_bytes.to_vec());
                    continue;
                };
                let mut job = decode_job(&pair, stored.value())?;
                drop(stored);
                if ready_at(&job) > now {
                    stale.push(rank_bytes.to_vec());
                    moved.push((pair, ready_at(&job)));
                    continue;
                }
                let old = ready_at(&job);
                job.lease_until = now.saturating_add(lease_for);
                job.attempt = job.attempt.saturating_add(1);
                job.rebuild = false;
                picked.push((pair, old, job));
            }
            for key in stale {
                ranks.remove(key.as_slice())?;
            }
            for (pair, ready) in moved {
                ranks.insert(ready_key(ready, &pair).as_slice(), &[][..])?;
            }
            let mut jobs = Vec::with_capacity(picked.len());
            for (pair, old, job) in picked {
                let bytes = encode_job(&job);
                table.insert(pair.as_slice(), bytes.as_slice())?;
                ranks.remove(ready_key(old, &pair).as_slice())?;
                ranks.insert(ready_key(ready_at(&job), &pair).as_slice(), &[][..])?;
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
            let mut ranks = write.open_table(RANK)?;
            let mut errors = write.open_table(ERRORS)?;
            let mut meta = write.open_table(WORK_META)?;
            prepare_errors(&table, &mut errors, &mut meta)?;
            for pair in done {
                let key = pair_key(pair.0, pair.1);
                let Some(stored) = table.get(key.as_slice())? else {
                    continue;
                };
                let mut job = decode_job(key.as_slice(), stored.value())?;
                drop(stored);
                let old_ready = ready_at(&job);
                let old_error = job.error.clone();
                if job.rebuild || again_set.contains(pair) {
                    job.rebuild = false;
                    job.lease_until = 0;
                    job.not_before = 0;
                    job.error.clear();
                    write_ranked(&mut table, &mut ranks, &key, &job, old_ready)?;
                    note_error(&meta, &mut errors, key.as_slice(), &old_error, "")?;
                } else {
                    table.remove(key.as_slice())?;
                    ranks.remove(ready_key(old_ready, &key).as_slice())?;
                    note_error(&meta, &mut errors, key.as_slice(), &old_error, "")?;
                }
            }
            for fail in failed {
                let key = pair_key(fail.token_in, fail.token_out);
                let Some(stored) = table.get(key.as_slice())? else {
                    continue;
                };
                let mut job = decode_job(key.as_slice(), stored.value())?;
                drop(stored);
                let old_ready = ready_at(&job);
                let old_error = job.error.clone();
                job.lease_until = 0;
                job.not_before = fail.not_before;
                job.error = clip_error(&fail.error);
                if job.rebuild {
                    job.rebuild = false;
                    job.not_before = 0;
                }
                write_ranked(&mut table, &mut ranks, &key, &job, old_ready)?;
                note_error(&meta, &mut errors, key.as_slice(), &old_error, &job.error)?;
            }
        }
        write.commit()?;
        Ok(())
    }
}

pub(crate) fn account_jobs(db: &redb::Database) -> eyre::Result<(u64, BTreeMap<String, u64>)> {
    ensure_errors(db)?;
    let jobs = count_jobs(db)?;
    let mut errors = BTreeMap::new();
    let read = db.begin_read()?;
    let table = match read.open_table(ERRORS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok((jobs, errors)),
        Err(err) => return Err(err.into()),
    };
    for item in table.iter()? {
        let (_, value) = item?;
        let error = String::from_utf8_lossy(value.value()).into_owned();
        if !error.is_empty() {
            *errors.entry(error).or_default() += 1;
        }
    }
    Ok((jobs, errors))
}

fn count_jobs(db: &redb::Database) -> eyre::Result<u64> {
    let read = db.begin_read()?;
    let table = match read.open_table(JOBS) {
        Ok(table) => table,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(err) => return Err(err.into()),
    };
    Ok(table.len()?)
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
        let mut ranks = write.open_table(RANK)?;
        let mut errors = write.open_table(ERRORS)?;
        let mut meta = write.open_table(WORK_META)?;
        prepare_errors(&table, &mut errors, &mut meta)?;
        for pair in gone {
            let key = pair_key(pair.0, pair.1);
            let Some(stored) = table.get(key.as_slice())? else {
                continue;
            };
            let job = decode_job(key.as_slice(), stored.value())?;
            drop(stored);
            let old_ready = ready_at(&job);
            let old_error = job.error.clone();
            if job.lease_until > now {
                let mut job = job;
                job.rebuild = true;
                write_ranked(&mut table, &mut ranks, &key, &job, old_ready)?;
            } else {
                table.remove(key.as_slice())?;
                ranks.remove(ready_key(old_ready, &key).as_slice())?;
                note_error(&meta, &mut errors, key.as_slice(), &old_error, "")?;
            }
        }
        for pair in alive {
            let key = pair_key(pair.0, pair.1);
            let (old_ready, old_error, job) = {
                let existing = table.get(key.as_slice())?;
                if let Some(stored) = existing {
                    let mut job = decode_job(key.as_slice(), stored.value())?;
                    drop(stored);
                    let old_ready = ready_at(&job);
                    let old_error = job.error.clone();
                    if job.lease_until > now {
                        job.rebuild = true;
                    } else {
                        job.not_before = 0;
                        job.error.clear();
                    }
                    (Some(old_ready), old_error, job)
                } else {
                    (
                        None,
                        String::new(),
                        ConduitJob {
                            token_in: pair.0,
                            token_out: pair.1,
                            lease_until: 0,
                            attempt: 0,
                            not_before: 0,
                            rebuild: false,
                            error: String::new(),
                        },
                    )
                }
            };
            let previous = old_ready.unwrap_or_else(|| ready_at(&job));
            write_ranked(&mut table, &mut ranks, &key, &job, previous)?;
            note_error(&meta, &mut errors, key.as_slice(), &old_error, &job.error)?;
        }
    }
    write.commit()?;
    Ok(())
}

fn ready_at(job: &ConduitJob) -> u64 {
    job.lease_until.max(job.not_before)
}

fn ready_key(ready: u64, pair: &[u8]) -> [u8; 48] {
    let mut key = [0_u8; 48];
    key[..8].copy_from_slice(&ready.to_be_bytes());
    let width = pair.len().min(40);
    key[8..8 + width].copy_from_slice(&pair[..width]);
    key
}

fn due_end(now: u64) -> [u8; 48] {
    let mut end = [0_u8; 48];
    end[..8].copy_from_slice(&now.saturating_add(1).to_be_bytes());
    end
}

fn write_ranked(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    ranks: &mut redb::Table<'_, &[u8], &[u8]>,
    key: &[u8; 40],
    job: &ConduitJob,
    old_ready: u64,
) -> eyre::Result<()> {
    let bytes = encode_job(job);
    table.insert(key.as_slice(), bytes.as_slice())?;
    let next = ready_at(job);
    if old_ready != next {
        ranks.remove(ready_key(old_ready, key).as_slice())?;
    }
    ranks.insert(ready_key(next, key).as_slice(), &[][..])?;
    Ok(())
}

fn prepare_errors(
    jobs: &redb::Table<'_, &[u8], &[u8]>,
    errors: &mut redb::Table<'_, &[u8], &[u8]>,
    meta: &mut redb::Table<'_, &str, u64>,
) -> eyre::Result<()> {
    if meta.get(ERRORS_READY)?.is_some() {
        return Ok(());
    }
    for item in jobs.iter()? {
        let (key, value) = item?;
        let job = decode_job(key.value(), value.value())?;
        if !job.error.is_empty() {
            errors.insert(key.value(), job.error.as_bytes())?;
        }
    }
    meta.insert(ERRORS_READY, 1)?;
    Ok(())
}

fn ensure_errors(db: &redb::Database) -> eyre::Result<()> {
    let read = db.begin_read()?;
    let ready = match read.open_table(WORK_META) {
        Ok(meta) => meta.get(ERRORS_READY)?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(err) => return Err(err.into()),
    };
    drop(read);
    if ready {
        return Ok(());
    }
    let mut write = db.begin_write()?;
    write.set_quick_repair(true);
    write.set_durability(Durability::Immediate)?;
    {
        let jobs = write.open_table(JOBS)?;
        let mut errors = write.open_table(ERRORS)?;
        let mut meta = write.open_table(WORK_META)?;
        prepare_errors(&jobs, &mut errors, &mut meta)?;
    }
    write.commit()?;
    Ok(())
}

fn note_error(
    meta: &redb::Table<'_, &str, u64>,
    errors: &mut redb::Table<'_, &[u8], &[u8]>,
    key: &[u8],
    old: &str,
    new: &str,
) -> eyre::Result<()> {
    if meta.get(ERRORS_READY)?.is_none() || old == new {
        return Ok(());
    }
    if new.is_empty() {
        errors.remove(key)?;
    } else {
        errors.insert(key, new.as_bytes())?;
    }
    Ok(())
}

fn clip_error(error: &str) -> String {
    let mut end = error.len().min(180);
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::super::super::catalog::Catalog;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use store_model::ConduitFail;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "ratatoskr-conduit-job-{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn pair(byte: u8) -> (alloy_primitives::Address, alloy_primitives::Address) {
        let mut left = [0_u8; 20];
        let mut right = [0_u8; 20];
        left[0] = 0x10;
        right[0] = 0x20;
        left[19] = byte;
        right[19] = byte;
        (
            alloy_primitives::Address::from(left),
            alloy_primitives::Address::from(right),
        )
    }

    #[test]
    fn a_claim_skips_a_leased_job_and_a_job_waiting_until_not_before() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let due = pair(1);
        let waiting = pair(2);
        super::sync_jobs(&catalog, &[due, waiting], &[], 0).unwrap();
        let claimed = catalog.claim_conduits(8, 10, 50).unwrap();
        assert_eq!(claimed.len(), 2);
        assert!(catalog.claim_conduits(8, 10, 50).unwrap().is_empty());
        catalog
            .finish_conduits(
                &[],
                &[],
                &[ConduitFail {
                    token_in: waiting.0,
                    token_out: waiting.1,
                    not_before: 200,
                    error: "wait".into(),
                }],
            )
            .unwrap();
        assert!(catalog.claim_conduits(8, 10, 50).unwrap().is_empty());
        let resumed = catalog.claim_conduits(8, 60, 10).unwrap();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].token_in, due.0);
        catalog.finish_conduits(&[due], &[], &[]).unwrap();
        assert!(catalog.claim_conduits(8, 100, 10).unwrap().is_empty());
        let ready = catalog.claim_conduits(8, 200, 10).unwrap();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].token_in, waiting.0);
    }

    #[test]
    fn a_failed_conduit_is_counted_until_the_job_is_removed() {
        let dir = TempDir::new();
        let catalog = Catalog::open(&dir.0).unwrap();
        let pair = pair(3);
        super::sync_jobs(&catalog, &[pair], &[], 0).unwrap();
        catalog
            .finish_conduits(
                &[],
                &[],
                &[ConduitFail {
                    token_in: pair.0,
                    token_out: pair.1,
                    not_before: 5,
                    error: "lens".into(),
                }],
            )
            .unwrap();
        let summary = catalog.state_summary().unwrap();
        assert_eq!(summary.jobs, 1);
        assert_eq!(summary.errors.get("lens").copied(), Some(1));
        super::sync_jobs(&catalog, &[], &[pair], u64::MAX).unwrap();
        let summary = catalog.state_summary().unwrap();
        assert_eq!(summary.jobs, 0);
        assert!(summary.errors.is_empty());
    }
}
