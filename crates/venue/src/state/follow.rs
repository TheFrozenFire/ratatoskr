//! What a committed snapshot enqueues next.

use super::{
    blank_job, concentrated_kind, curve_is_current, curve_job, detail_id, join_detail, limits,
    priority_of, unix_now, CurveRecord, PoolDetail, VenueStore, WorkFail, WorkJob, CLASS_NONE,
    CLASS_TICKS, DIRTY_BAND, STAGE_CURVE, STAGE_MUTABLE, STAGE_TICKS,
};
use alloy_primitives::B256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    pub retry_mutable: bool,
    pub stage: Option<u8>,
}

/// What to enqueue after a mutable snapshot commits.
///
/// A head refresh with the same bitmap keeps tick pages and derives a curve.
/// A mint or a changed bitmap has to read ticks again. A truncated bitmap
/// stays on the mutable stage.
pub fn plan_follow_up(
    class: u8,
    bitmap_changed: bool,
    truncated: bool,
    tick_count: u32,
    _canonical: bool,
) -> Plan {
    if truncated {
        return Plan {
            retry_mutable: true,
            stage: None,
        };
    }
    let reread = tick_count > 0 && (bitmap_changed || class == CLASS_TICKS || class == CLASS_NONE);
    if reread {
        return Plan {
            retry_mutable: false,
            stage: Some(STAGE_TICKS),
        };
    }
    Plan {
        retry_mutable: false,
        stage: Some(STAGE_CURVE),
    }
}

pub(super) struct FollowPlan {
    pub(super) job: Option<WorkJob>,
    pub(super) derive: bool,
    pub(super) priority: u64,
}

pub(super) struct FollowInput<'a> {
    pub(super) kind: u8,
    pub(super) id: B256,
    pub(super) detail: &'a PoolDetail,
    pub(super) class: u8,
    pub(super) bitmap_changed: bool,
    pub(super) truncated: bool,
    pub(super) tick_count: u32,
    pub(super) generation: u64,
    pub(super) watched: bool,
    pub(super) book_ready: bool,
}

/// What to enqueue after the detail lens returns.
///
/// Outstanding tick pages are still walked. `max_in` for a concentrated book
/// is the ladder, and that ladder needs the tick words the bitmap only names.
pub(super) fn follow_for_detail(input: &FollowInput<'_>) -> FollowPlan {
    let cap = crate::fit::trade_capacity(input.detail);
    let priority = priority_of(input.watched, cap.top);
    if input.truncated {
        let mut job = blank_job(STAGE_MUTABLE, input.kind, input.id);
        job.priority = priority;
        job.class = input.class;
        return FollowPlan {
            job: Some(job),
            derive: false,
            priority,
        };
    }
    let outstanding = !input.book_ready && concentrated_kind(input.kind) && input.tick_count > 0;
    if outstanding {
        let plan: super::Plan = super::plan_follow_up(
            input.class,
            input.bitmap_changed,
            false,
            input.tick_count,
            true,
        );
        let Some(stage) = plan.stage.filter(|stage| *stage == STAGE_TICKS) else {
            return FollowPlan {
                job: None,
                derive: cap.any_positive,
                priority,
            };
        };
        let mut job = blank_job(stage, input.kind, input.id);
        job.generation = input.generation;
        job.priority = priority;
        job.class = input.class;
        return FollowPlan {
            job: Some(job),
            derive: false,
            priority,
        };
    }
    if !cap.any_positive {
        return FollowPlan {
            job: None,
            derive: false,
            priority,
        };
    }
    FollowPlan {
        job: None,
        derive: true,
        priority,
    }
}

/// Local curve once the tick words for this generation are stored.
pub(super) async fn after_book<S: VenueStore + crate::ConduitStore>(
    store: &S,
    job: &WorkJob,
) -> eyre::Result<Option<WorkJob>> {
    let state = store.read_state(job.kind, job.id).await?;
    let Some(mutable) = state.mutable else {
        return Ok(Some(blank_job(STAGE_MUTABLE, job.kind, job.id)));
    };
    let Some(immutable) = state.immutable else {
        return Ok(None);
    };
    let detail = match join_detail(&immutable, &mutable.body, &state.ticks, job.kind, job.id) {
        Ok(detail) => detail,
        Err(_) => {
            if curve_is_current(state.curve.as_ref(), mutable.generation) {
                return Ok(None);
            }
            let mut next = curve_job(job.kind, job.id, mutable.generation);
            next.priority = job.priority;
            return Ok(Some(next));
        }
    };
    let cap = crate::fit::trade_capacity(&detail);
    if !cap.any_positive || curve_is_current(state.curve.as_ref(), mutable.generation) {
        return Ok(None);
    }
    let priority = priority_of((job.priority & DIRTY_BAND) != 0, cap.top);
    match curve_for_snapshot(&detail, mutable.generation) {
        Ok(curve) => {
            if crate::conduit::commit_curve_store(store, &curve)
                .await
                .is_err()
            {
                let mut next = curve_job(job.kind, job.id, mutable.generation);
                next.priority = priority;
                return Ok(Some(next));
            }
            Ok(None)
        }
        Err(_) => {
            let mut next = curve_job(job.kind, job.id, mutable.generation);
            next.priority = priority;
            Ok(Some(next))
        }
    }
}

/// Choose a curve from local quotes of `detail`.
pub fn curve_for_snapshot(detail: &PoolDetail, generation: u64) -> Result<CurveRecord, String> {
    let (kind, id) = detail_id(detail)?;
    Ok(CurveRecord {
        kind,
        id,
        generation,
        version: super::CURVE_VERSION,
        payload: crate::fit::local_curve_payload(detail)?,
    })
}

pub(crate) fn keep_or_drop(
    job: &WorkJob,
    error: String,
    give_up_after: u32,
    done: &mut Vec<(u8, u8, B256)>,
    failed: &mut Vec<WorkFail>,
    notes: &mut Vec<String>,
) {
    let error = chain::rpc::redact_secret(&error);
    let kind = crate::catalog::kind_name(job.kind);
    if give_up_after > 0 && job.attempt >= give_up_after {
        observability::error!(
            stage = job.stage,
            kind,
            id = %job.id,
            attempt = job.attempt,
            %error,
            "state gave up"
        );
        notes.push(format!("gave up {kind} {}: {error}", job.id));
        done.push((job.stage, job.kind, job.id));
        return;
    }
    observability::warn!(
        stage = job.stage,
        kind,
        id = %job.id,
        attempt = job.attempt,
        %error,
        "state job failed"
    );
    failed.push(WorkFail {
        stage: job.stage,
        kind: job.kind,
        id: job.id,
        not_before: unix_now().saturating_add(limits().retry_secs),
        error,
    });
}
