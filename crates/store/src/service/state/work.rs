//! Work queue in `work.redb`.

use alloy_primitives::{B256, U256};
use redb::{Durability, ReadableDatabase, ReadableTable};
use std::time::{SystemTime, UNIX_EPOCH};
use store_model::{
    decode_job, encode_job, priority_of, tick_page_generation, WorkFail, WorkJob, CLASS_NONE,
    STAGE_IMMUTABLE, STAGE_MUTABLE, STAGE_QUOTE, TICK_PAGE,
};

use super::super::catalog::Catalog;
use super::*;

pub(super) const JOB_STAGE: TableDefinition<&[u8], u64> = TableDefinition::new("job_stage");

pub(super) const JOB_ERROR: TableDefinition<&[u8], &[u8]> = TableDefinition::new("job_error");

pub(super) const WORK_META: TableDefinition<&str, u64> = TableDefinition::new("work_meta");

const ACCOUNTS_READY: &str = "job_accounts";

impl Catalog {
    /// Write `jobs` and clear every other stage from immutable through quote
    /// for each identity in `rows`. A leased job is left alone.
    pub fn reconcile(&self, rows: &[(u8, B256)], jobs: &[WorkJob], now: u64) -> eyre::Result<u64> {
        let mut drop = Vec::new();
        for (kind, id) in rows {
            for stage in STAGE_IMMUTABLE..=STAGE_QUOTE {
                if jobs
                    .iter()
                    .any(|job| job.stage == stage && job.kind == *kind && job.id == *id)
                {
                    continue;
                }
                drop.push((stage, *kind, *id));
            }
        }
        self.sync_jobs(&drop, jobs, now)
    }

    pub fn claim_work(
        &self,
        stage: u8,
        limit: u32,
        now: u64,
        lease_for: u64,
    ) -> eyre::Result<Vec<WorkJob>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let jobs = {
            let mut table = write.open_table(JOBS)?;
            let mut ranks = write.open_table(JOB_RANK)?;
            let mut stages = write.open_table(JOB_STAGE)?;
            let mut errors = write.open_table(JOB_ERROR)?;
            let meta = write.open_table(WORK_META)?;
            let mut picked = Vec::new();
            let mut stale = Vec::new();
            let start = rank_key(stage, u64::MAX, 0, &B256::ZERO);
            let end = rank_key(stage.saturating_add(1), u64::MAX, 0, &B256::ZERO);
            let mut saw_rank = false;
            for item in ranks.range::<&[u8]>(start.as_slice()..end.as_slice())? {
                saw_rank = true;
                if picked.len() == limit as usize {
                    break;
                }
                let (key, _) = item?;
                let Some((rank_stage, priority, kind, id)) = decode_rank(key.value()) else {
                    continue;
                };
                let identity = job_key(rank_stage, kind, &id);
                let Some(stored) = table.get(identity.as_slice())? else {
                    stale.push(key.value().to_vec());
                    continue;
                };
                let mut job = decode_job(identity.as_slice(), stored.value())?;
                drop(stored);
                if job.priority != priority {
                    stale.push(key.value().to_vec());
                    continue;
                }
                if !lease_job(&mut job, now, lease_for) {
                    continue;
                }
                picked.push((identity.to_vec(), job));
            }
            if !saw_rank {
                let start = job_key(stage, 0, &B256::ZERO);
                let end = job_key(stage.saturating_add(1), 0, &B256::ZERO);
                for item in table.range::<&[u8]>(start.as_slice()..end.as_slice())? {
                    if picked.len() == limit as usize {
                        break;
                    }
                    let (key, value) = item?;
                    let mut job = decode_job(key.value(), value.value())?;
                    if !lease_job(&mut job, now, lease_for) {
                        continue;
                    }
                    picked.push((key.value().to_vec(), job));
                }
            }
            for key in stale {
                ranks.remove(key.as_slice())?;
            }
            let mut jobs = Vec::with_capacity(picked.len());
            for (_key, job) in picked {
                put_job(
                    &mut table,
                    &mut ranks,
                    &mut stages,
                    &mut errors,
                    &meta,
                    &job,
                )?;
                jobs.push(job);
            }
            jobs
        };
        write.commit()?;
        Ok(jobs)
    }

    pub fn enqueue_work(&self, jobs: &[WorkJob], now: u64) -> eyre::Result<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        self.sync_jobs(&[], jobs, now)?;
        Ok(())
    }

    pub fn finish_work(&self, done: &[(u8, u8, B256)], failed: &[WorkFail]) -> eyre::Result<()> {
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut table = write.open_table(JOBS)?;
            let mut ranks = write.open_table(JOB_RANK)?;
            let mut stages = write.open_table(JOB_STAGE)?;
            let mut errors = write.open_table(JOB_ERROR)?;
            let mut meta = write.open_table(WORK_META)?;
            prepare_state_accounts(&table, &mut stages, &mut errors, &mut meta)?;
            for (stage, kind, id) in done {
                clear_job(
                    &mut table,
                    &mut ranks,
                    &mut stages,
                    &mut errors,
                    &meta,
                    *stage,
                    *kind,
                    id,
                )?;
            }
            for fail in failed {
                let key = job_key(fail.stage, fail.kind, &fail.id);
                let Some(stored) = table.get(key.as_slice())? else {
                    continue;
                };
                let mut job = decode_job(key.as_slice(), stored.value())?;
                drop(stored);
                job.lease_until = 0;
                job.not_before = fail.not_before;
                job.error = clip_error(&fail.error);
                put_job(
                    &mut table,
                    &mut ranks,
                    &mut stages,
                    &mut errors,
                    &meta,
                    &job,
                )?;
            }
        }
        write.commit()?;
        Ok(())
    }

    /// One mutable job per pool the watch just marked, at the dirty priority.
    ///
    /// The entity commit has already landed. A leased job is left alone.
    pub(super) fn enqueue_bumped(&self, bumped: &[(u8, B256, u8)]) -> eyre::Result<()> {
        if bumped.is_empty() {
            return Ok(());
        }
        let mut jobs = Vec::with_capacity(bumped.len());
        for (kind, id, class) in bumped {
            let state = self.read_state(*kind, *id)?;
            let top = super::super::hooks::capacity(*kind, *id, &state)?
                .map(|cap| cap.top)
                .unwrap_or(U256::ZERO);
            let mut job = blank_job(STAGE_MUTABLE, *kind, *id);
            job.class = *class;
            job.priority = priority_of(true, top);
            if let Some(mutable) = &state.mutable {
                job.generation = mutable.generation;
            }
            jobs.push(job);
        }
        self.enqueue_work(&jobs, unix_now())
    }

    pub fn first_missing_tick_page(
        &self,
        kind: u8,
        id: B256,
        generation: u64,
        tick_count: u32,
    ) -> eyre::Result<u32> {
        let pages = tick_count.div_ceil(TICK_PAGE);
        let read = self.entities.begin_read()?;
        let table = match read.open_table(TICK_PAGES) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
            Err(err) => return Err(err.into()),
        };
        for page in 0..pages {
            let key = tick_key(kind, id, page);
            match table.get(key.as_slice())? {
                None => return Ok(page),
                Some(value) => {
                    let stored = tick_page_generation(value.value())?;
                    if stored != generation {
                        return Ok(page);
                    }
                }
            }
        }
        Ok(pages)
    }

    fn sync_jobs(&self, drop: &[(u8, u8, B256)], jobs: &[WorkJob], now: u64) -> eyre::Result<u64> {
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        let written = {
            let mut table = write.open_table(JOBS)?;
            let mut ranks = write.open_table(JOB_RANK)?;
            let mut stages = write.open_table(JOB_STAGE)?;
            let mut errors = write.open_table(JOB_ERROR)?;
            let mut meta = write.open_table(WORK_META)?;
            prepare_state_accounts(&table, &mut stages, &mut errors, &mut meta)?;
            for (stage, kind, id) in drop {
                let Some(existing) = load_job(&table, *stage, *kind, id)? else {
                    continue;
                };
                if existing.lease_until > now {
                    ensure_rank(&mut ranks, &existing)?;
                    continue;
                }
                clear_job(
                    &mut table,
                    &mut ranks,
                    &mut stages,
                    &mut errors,
                    &meta,
                    *stage,
                    *kind,
                    id,
                )?;
            }
            let mut written = 0_u64;
            for job in jobs {
                if let Some(existing) = load_job(&table, job.stage, job.kind, &job.id)? {
                    if existing.lease_until > now {
                        ensure_rank(&mut ranks, &existing)?;
                        continue;
                    }
                    if existing.priority != job.priority {
                        remove_rank(&mut ranks, &existing)?;
                    }
                }
                put_job(&mut table, &mut ranks, &mut stages, &mut errors, &meta, job)?;
                written += 1;
            }
            written
        };
        write.commit()?;
        Ok(written)
    }

    /// Give every stored job a rank key. Startup finds leased rows the venue
    /// walk did not rewrite.
    pub fn index_ranks(&self) -> eyre::Result<()> {
        let read = self.work.begin_read()?;
        let jobs = match read.open_table(JOBS) {
            Ok(table) => table,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let ranks = match read.open_table(JOB_RANK) {
            Ok(table) => Some(table),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(err) => return Err(err.into()),
        };
        let mut missing = Vec::new();
        for item in jobs.iter()? {
            let (key, value) = item?;
            let job = decode_job(key.value(), value.value())?;
            let rank = rank_key(job.stage, job.priority, job.kind, &job.id);
            let present = match &ranks {
                Some(table) => table.get(rank.as_slice())?.is_some(),
                None => false,
            };
            if !present {
                missing.push(rank);
            }
        }
        drop(jobs);
        drop(ranks);
        drop(read);
        if missing.is_empty() {
            return Ok(());
        }
        let mut write = self.work.begin_write()?;
        write.set_durability(Durability::Immediate)?;
        {
            let mut ranks = write.open_table(JOB_RANK)?;
            for key in &missing {
                ranks.insert(key.as_slice(), &[][..])?;
            }
        }
        write.commit()?;
        Ok(())
    }
}

fn blank_job(stage: u8, kind: u8, id: B256) -> WorkJob {
    WorkJob {
        stage,
        kind,
        id,
        lease_until: 0,
        attempt: 0,
        not_before: 0,
        generation: 0,
        tick_cursor: 0,
        class: CLASS_NONE,
        priority: 0,
        error: String::new(),
    }
}

pub(crate) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn job_key(stage: u8, kind: u8, id: &B256) -> [u8; 34] {
    let mut out = [0u8; 34];
    out[0] = stage;
    out[1] = kind;
    out[2..].copy_from_slice(id.as_slice());
    out
}

fn rank_key(stage: u8, priority: u64, kind: u8, id: &B256) -> [u8; 42] {
    let mut out = [0u8; 42];
    out[0] = stage;
    let inverted = u64::MAX - priority;
    out[1..9].copy_from_slice(&inverted.to_be_bytes());
    out[9] = kind;
    out[10..].copy_from_slice(id.as_slice());
    out
}

fn decode_rank(key: &[u8]) -> Option<(u8, u64, u8, B256)> {
    if key.len() != 42 {
        return None;
    }
    let inverted = u64::from_be_bytes(key[1..9].try_into().ok()?);
    Some((
        key[0],
        u64::MAX - inverted,
        key[9],
        B256::from_slice(&key[10..]),
    ))
}

fn lease_job(job: &mut WorkJob, now: u64, lease_for: u64) -> bool {
    if job.lease_until > now || job.not_before > now {
        return false;
    }
    job.lease_until = now.saturating_add(lease_for);
    job.attempt = job.attempt.saturating_add(1);
    true
}

fn load_job(
    table: &redb::Table<'_, &[u8], &[u8]>,
    stage: u8,
    kind: u8,
    id: &B256,
) -> eyre::Result<Option<WorkJob>> {
    let key = job_key(stage, kind, id);
    let Some(stored) = table.get(key.as_slice())? else {
        return Ok(None);
    };
    Ok(Some(decode_job(key.as_slice(), stored.value())?))
}

fn put_job(
    jobs: &mut redb::Table<'_, &[u8], &[u8]>,
    ranks: &mut redb::Table<'_, &[u8], &[u8]>,
    stages: &mut redb::Table<'_, &[u8], u64>,
    errors: &mut redb::Table<'_, &[u8], &[u8]>,
    meta: &redb::Table<'_, &str, u64>,
    job: &WorkJob,
) -> eyre::Result<()> {
    let key = job_key(job.stage, job.kind, &job.id);
    let previous = load_job(jobs, job.stage, job.kind, &job.id)?;
    let bytes = encode_job(job);
    jobs.insert(key.as_slice(), bytes.as_slice())?;
    let rank = rank_key(job.stage, job.priority, job.kind, &job.id);
    ranks.insert(rank.as_slice(), &[][..])?;
    note_state_job(stages, errors, meta, &key, previous.as_ref(), Some(job))?;
    Ok(())
}

fn remove_rank(ranks: &mut redb::Table<'_, &[u8], &[u8]>, job: &WorkJob) -> eyre::Result<()> {
    let rank = rank_key(job.stage, job.priority, job.kind, &job.id);
    ranks.remove(rank.as_slice())?;
    Ok(())
}

fn ensure_rank(ranks: &mut redb::Table<'_, &[u8], &[u8]>, job: &WorkJob) -> eyre::Result<()> {
    let rank = rank_key(job.stage, job.priority, job.kind, &job.id);
    if ranks.get(rank.as_slice())?.is_none() {
        ranks.insert(rank.as_slice(), &[][..])?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn clear_job(
    jobs: &mut redb::Table<'_, &[u8], &[u8]>,
    ranks: &mut redb::Table<'_, &[u8], &[u8]>,
    stages: &mut redb::Table<'_, &[u8], u64>,
    errors: &mut redb::Table<'_, &[u8], &[u8]>,
    meta: &redb::Table<'_, &str, u64>,
    stage: u8,
    kind: u8,
    id: &B256,
) -> eyre::Result<()> {
    let key = job_key(stage, kind, id);
    let Some(stored) = jobs.remove(key.as_slice())? else {
        return Ok(());
    };
    if let Ok(job) = decode_job(key.as_slice(), stored.value()) {
        remove_rank(ranks, &job)?;
        note_state_job(stages, errors, meta, &key, Some(&job), None)?;
    }
    Ok(())
}

pub(super) fn ensure_state_accounts(db: &redb::Database) -> eyre::Result<()> {
    let read = db.begin_read()?;
    let ready = match read.open_table(WORK_META) {
        Ok(meta) => meta.get(ACCOUNTS_READY)?.is_some(),
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
        let mut stages = write.open_table(JOB_STAGE)?;
        let mut errors = write.open_table(JOB_ERROR)?;
        let mut meta = write.open_table(WORK_META)?;
        prepare_state_accounts(&jobs, &mut stages, &mut errors, &mut meta)?;
    }
    write.commit()?;
    Ok(())
}

fn prepare_state_accounts(
    jobs: &redb::Table<'_, &[u8], &[u8]>,
    stages: &mut redb::Table<'_, &[u8], u64>,
    errors: &mut redb::Table<'_, &[u8], &[u8]>,
    meta: &mut redb::Table<'_, &str, u64>,
) -> eyre::Result<()> {
    if meta.get(ACCOUNTS_READY)?.is_some() {
        return Ok(());
    }
    let mut counts: std::collections::BTreeMap<u8, u64> = std::collections::BTreeMap::new();
    for item in jobs.iter()? {
        let (key, value) = item?;
        let job = decode_job(key.value(), value.value())?;
        *counts.entry(job.stage).or_default() += 1;
        if !job.error.is_empty() {
            errors.insert(key.value(), job.error.as_bytes())?;
        }
    }
    for (stage, count) in counts {
        stages.insert(stage.to_be_bytes().as_slice(), count)?;
    }
    meta.insert(ACCOUNTS_READY, 1)?;
    Ok(())
}

fn note_state_job(
    stages: &mut redb::Table<'_, &[u8], u64>,
    errors: &mut redb::Table<'_, &[u8], &[u8]>,
    meta: &redb::Table<'_, &str, u64>,
    key: &[u8; 34],
    previous: Option<&WorkJob>,
    next: Option<&WorkJob>,
) -> eyre::Result<()> {
    if meta.get(ACCOUNTS_READY)?.is_none() {
        return Ok(());
    }
    if previous.is_none() {
        if let Some(job) = next {
            let stage = job.stage.to_be_bytes();
            let count = stages
                .get(stage.as_slice())?
                .map(|value| value.value())
                .unwrap_or(0);
            stages.insert(stage.as_slice(), count.saturating_add(1))?;
        }
    }
    if next.is_none() {
        if let Some(job) = previous {
            let stage = job.stage.to_be_bytes();
            let count = stages
                .get(stage.as_slice())?
                .map(|value| value.value())
                .unwrap_or(0);
            stages.insert(stage.as_slice(), count.saturating_sub(1))?;
        }
    }
    let old = previous.map(|job| job.error.as_str()).unwrap_or("");
    let new = next.map(|job| job.error.as_str()).unwrap_or("");
    if old != new {
        if new.is_empty() {
            errors.remove(key.as_slice())?;
        } else {
            errors.insert(key.as_slice(), new.as_bytes())?;
        }
    }
    Ok(())
}

fn clip_error(error: &str) -> String {
    error.chars().take(160).collect()
}
