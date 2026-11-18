//! Which stage a stored pool still needs.
//!
//! An existing lease is left alone. This is the resume path after `work.redb`
//! is deleted, not the steady-state loop.

use super::{
    blank_job, concentrated_kind, curve_is_current, curve_job, join_detail, PoolState, VenueRow,
    WorkJob, CLASS_NONE, STAGE_IMMUTABLE, STAGE_MUTABLE, STAGE_TICKS, TICK_PAGE,
};
use crate::{Catalog, VenueStore};
use alloy_primitives::{B256, U256};
use store_model::{priority_of, MutableView, METRIC};

const PAGE: usize = 256;

enum Planned {
    Job(Option<WorkJob>),
    NeedPage { book: u64, tick_count: u32 },
}

/// Insert jobs for pools whose stored state is behind.
pub(crate) fn reconcile_catalog(catalog: &Catalog, now: u64) -> eyre::Result<u64> {
    let mut inserted = 0_u64;
    for kind in 1..=METRIC {
        let mut after = None;
        loop {
            let page = catalog.page_venues(kind, after, PAGE)?;
            if page.is_empty() {
                break;
            }
            after = Some(page.last().expect("page").id);
            let (rows, jobs) = jobs_for_page(
                &page,
                |row| catalog.read_state(row.kind, row.id),
                |row, generation, tick_count| {
                    catalog.first_missing_tick_page(row.kind, row.id, generation, tick_count)
                },
            )?;
            inserted += catalog.reconcile(&rows, &jobs, now)?;
            if page.len() < PAGE {
                break;
            }
        }
    }
    catalog.index_ranks()?;
    Ok(inserted)
}

/// Insert jobs for pools whose stored state is behind.
pub(super) async fn reconcile<S: VenueStore>(store: &S, now: u64) -> eyre::Result<u64> {
    let mut inserted = 0_u64;
    for kind in 1..=METRIC {
        let mut after = None;
        loop {
            let page = store.page_venues(kind, after, PAGE).await?;
            if page.is_empty() {
                break;
            }
            after = Some(page.last().expect("page").id);
            let mut rows = Vec::with_capacity(page.len());
            let mut jobs = Vec::new();
            for row in &page {
                let state = store.read_state(row.kind, row.id).await?;
                rows.push((row.kind, row.id));
                let job = match plan_job(row, &state, None) {
                    Planned::Job(job) => job,
                    Planned::NeedPage {
                        book, tick_count, ..
                    } => {
                        let cursor = store
                            .first_missing_tick_page(row.kind, row.id, book, tick_count)
                            .await?;
                        match plan_job(row, &state, Some(cursor)) {
                            Planned::Job(job) => job,
                            Planned::NeedPage { .. } => {
                                return Err(eyre::eyre!("tick page"));
                            }
                        }
                    }
                };
                if let Some(job) = job {
                    jobs.push(job);
                }
            }
            inserted += store.reconcile(&rows, &jobs, now).await?;
            if page.len() < PAGE {
                break;
            }
        }
    }
    store.index_ranks().await?;
    Ok(inserted)
}

fn jobs_for_page(
    page: &[VenueRow],
    mut read_state: impl FnMut(&VenueRow) -> eyre::Result<PoolState>,
    mut missing: impl FnMut(&VenueRow, u64, u32) -> eyre::Result<u32>,
) -> eyre::Result<(Vec<(u8, B256)>, Vec<WorkJob>)> {
    let mut rows = Vec::with_capacity(page.len());
    let mut jobs = Vec::new();
    for row in page {
        let state = read_state(row)?;
        rows.push((row.kind, row.id));
        let job = match plan_job(row, &state, None) {
            Planned::Job(job) => job,
            Planned::NeedPage {
                book, tick_count, ..
            } => {
                let cursor = missing(row, book, tick_count)?;
                match plan_job(row, &state, Some(cursor)) {
                    Planned::Job(job) => job,
                    Planned::NeedPage { .. } => return Err(eyre::eyre!("tick page")),
                }
            }
        };
        if let Some(job) = job {
            jobs.push(job);
        }
    }
    Ok((rows, jobs))
}

fn plan_job(row: &VenueRow, state: &PoolState, page: Option<u32>) -> Planned {
    if state.immutable.is_none() {
        return Planned::Job(Some(blank_job(STAGE_IMMUTABLE, row.kind, row.id)));
    }
    let Some(mutable) = &state.mutable else {
        return Planned::Job(Some(blank_job(STAGE_MUTABLE, row.kind, row.id)));
    };
    if state.dirty.is_some() || mutable.truncated {
        let top = snapshot_capacity(row.kind, row.id, state)
            .map(|(_, _, top)| top)
            .unwrap_or(U256::ZERO);
        let mut job = blank_job(STAGE_MUTABLE, row.kind, row.id);
        job.generation = mutable.generation;
        job.class = state
            .dirty
            .as_ref()
            .map(|dirty| dirty.class)
            .unwrap_or(CLASS_NONE);
        job.priority = priority_of(state.dirty.is_some(), top);
        return Planned::Job(Some(job));
    }
    let top = match snapshot_capacity(row.kind, row.id, state) {
        Some((any_positive, fills_sample, top)) => {
            if needs_ticks(row.kind, mutable) {
                let Some(cursor) = page else {
                    return Planned::NeedPage {
                        book: mutable.book,
                        tick_count: mutable.tick_count,
                    };
                };
                let pages = mutable.tick_count.div_ceil(TICK_PAGE);
                if cursor < pages {
                    let mut job = blank_job(STAGE_TICKS, row.kind, row.id);
                    job.generation = mutable.generation;
                    job.tick_cursor = i32::try_from(cursor).unwrap_or(i32::MAX);
                    job.priority = priority_of(false, top);
                    return Planned::Job(Some(job));
                }
            }
            if !any_positive {
                return Planned::Job(None);
            }
            if !fills_sample {
                if curve_is_current(state.curve.as_ref(), mutable.generation) {
                    return Planned::Job(None);
                }
                let mut job = curve_job(row.kind, row.id, mutable.generation);
                job.priority = priority_of(false, top);
                return Planned::Job(Some(job));
            }
            top
        }
        None => return structural_job(row, mutable, state, 0, page),
    };
    structural_job(row, mutable, state, priority_of(false, top), page)
}

/// Tick, then a local curve. Chain samples are not requested.
fn structural_job(
    row: &VenueRow,
    mutable: &MutableView,
    state: &PoolState,
    priority: u64,
    page: Option<u32>,
) -> Planned {
    if needs_ticks(row.kind, mutable) {
        let Some(cursor) = page else {
            return Planned::NeedPage {
                book: mutable.book,
                tick_count: mutable.tick_count,
            };
        };
        let pages = mutable.tick_count.div_ceil(TICK_PAGE);
        if cursor < pages {
            let mut job = blank_job(STAGE_TICKS, row.kind, row.id);
            job.generation = mutable.generation;
            job.tick_cursor = i32::try_from(cursor).unwrap_or(i32::MAX);
            job.priority = priority;
            return Planned::Job(Some(job));
        }
    }
    if !curve_is_current(state.curve.as_ref(), mutable.generation) {
        let mut job = curve_job(row.kind, row.id, mutable.generation);
        job.priority = priority;
        return Planned::Job(Some(job));
    }
    Planned::Job(None)
}

fn needs_ticks(kind: u8, mutable: &MutableView) -> bool {
    concentrated_kind(kind) && mutable.tick_count > 0 && !mutable.truncated
}

fn snapshot_capacity(kind: u8, id: B256, state: &PoolState) -> Option<(bool, bool, U256)> {
    let immutable = state.immutable.as_ref()?;
    let mutable = state.mutable.as_ref()?;
    let detail = join_detail(immutable, &mutable.body, &state.ticks, kind, id).ok()?;
    let cap = crate::fit::trade_capacity(&detail);
    Some((cap.any_positive, cap.fills_sample, cap.top))
}
