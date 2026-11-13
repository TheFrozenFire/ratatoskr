//! One pass of the state pipeline and the watch.
//!
//! A detail read uses the latest stored header. `pin_block` holds that read
//! at one block for the whole run.

use super::*;

/// One pass of the state pipeline. The caller loops until shutdown.
///
/// A pass reconciles only when asked. Each pass runs every stage that has
/// work ready. Ticks and the curve are not held behind an earlier stage.
pub async fn run<S, P>(
    store: &S,
    peer: Option<&P>,
    opts: &RunOpts,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<RunReport>
where
    S: EntityStore + Clone + 'static,
    P: HistoryPeer + Clone + 'static,
{
    let mut report = RunReport::default();
    if *stop.borrow() {
        return Ok(report);
    }
    let inserted = store.reconcile(unix_now()).await?;
    report.notes.push(format!("reconcile {inserted}"));
    tracing::info!(inserted, "state reconcile");
    let queued = store.enqueue_conduit_fill().await?;
    report.notes.push(format!("conduit fill {queued}"));
    tracing::info!(queued, "conduit fill");
    let urls = if opts.rpcs.is_empty() {
        crate::rpc::default_call_rpcs()
    } else {
        opts.rpcs.clone()
    };
    // reqwest's blocking client builds a shell runtime. Doing that on a
    // worker panics in debug. The blocking pool is allowed to block.
    let rpc = tokio::task::spawn_blocking(move || Rpc::new(urls))
        .await
        .wrap_err("rpc client task")??;
    let mut block = current_block(store, &rpc, opts.pin_block).await?;
    crate::metrics::set_block(block);
    tracing::info!(block, "state block");
    let give_up_after = if opts.until_idle { 4 } else { 0 };
    let mut quiet = 0_u32;
    let mut batches = 0_u64;
    loop {
        if crate::hold_for_pause(&stop).await {
            return Ok(report);
        }
        if let Some(peer) = peer {
            match watch_once(store, peer).await {
                Ok(note) => {
                    if let Some(note) = note {
                        tracing::info!(%note, "watch");
                    }
                }
                Err(err) => {
                    let note = format!("watch: {err:#}");
                    tracing::error!(%note);
                    report.notes.push(note);
                }
            }
        }
        if crate::hold_for_pause(&stop).await {
            return Ok(report);
        }
        let next = current_block(store, &rpc, opts.pin_block).await?;
        if next != block {
            block = next;
            crate::metrics::set_block(block);
            tracing::info!(block, "state block");
        }
        let mut notes = Vec::new();
        let worked = drive_one(store, &rpc, block, unix_now(), give_up_after, &mut notes).await?;
        report.notes.extend(notes);
        if worked {
            quiet = 0;
            batches += 1;
            if batches.is_multiple_of(25) {
                tracing::info!(batches, "state progress");
            }
            continue;
        }
        if opts.until_idle {
            quiet += 1;
            if quiet >= 2 {
                report.notes.push("state idle".into());
                tracing::info!(batches, "state idle");
                return Ok(report);
            }
            tokio::select! {
                _ = stop.changed() => {}
                _ = tokio::time::sleep(Duration::from_secs(RETRY_SECS + 5)) => {}
            }
            continue;
        }
        sleep_or_stop(&mut stop).await;
    }
}

/// The block a detail read uses. A pin wins. Otherwise the block is the
/// latest stored header. An empty chain has no header yet.
fn tagged_block(pin: Option<u64>, cursor: u64) -> Option<u64> {
    if let Some(pin) = pin {
        return Some(pin);
    }
    cursor.checked_sub(1)
}

async fn current_block<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    pin: Option<u64>,
) -> eyre::Result<u64> {
    if let Some(block) = tagged_block(pin, store.header_cursor().await?) {
        return Ok(block);
    }
    let rpc = rpc.clone();
    tokio::task::spawn_blocking(move || rpc.block_number())
        .await
        .wrap_err("block number task")?
}

async fn sleep_or_stop(stop: &mut tokio::sync::watch::Receiver<bool>) {
    tokio::select! {
        _ = stop.changed() => {}
        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
    }
}

async fn drive_one<S>(
    store: &S,
    rpc: &Rpc,
    block: u64,
    now: u64,
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<bool>
where
    S: EntityStore + Clone + 'static,
{
    if crate::work_paused() {
        return Ok(false);
    }
    let mut claimed = Vec::new();
    for stage in [STAGE_IMMUTABLE, STAGE_MUTABLE, STAGE_TICKS, STAGE_CURVE] {
        let jobs = store
            .claim_work(stage, DETAIL_BATCH, now, LEASE_SECS)
            .await?;
        if !jobs.is_empty() {
            claimed.push((stage, jobs));
        }
    }
    let mut worked = false;
    if !claimed.is_empty() {
        let handle = tokio::runtime::Handle::current();
        let mut threads = Vec::with_capacity(claimed.len());
        for (stage, jobs) in claimed {
            let store = store.clone();
            let rpc = rpc.clone();
            let handle = handle.clone();
            threads.push(std::thread::spawn(move || {
                handle.block_on(run_stage(
                    store,
                    rpc,
                    stage,
                    jobs,
                    block,
                    now,
                    give_up_after,
                ))
            }));
        }
        let mut failed = None;
        for thread in threads {
            match thread.join() {
                Ok(Ok(batch_notes)) => notes.extend(batch_notes),
                Ok(Err(err)) if failed.is_none() => failed = Some(err),
                Ok(Err(_)) => {}
                Err(_) => {
                    if failed.is_none() {
                        failed = Some(eyre::eyre!("stage task panicked"));
                    }
                }
            }
        }
        if let Some(err) = failed {
            return Err(err);
        }
        worked = true;
    }
    if drive_conduits(store, now, give_up_after, notes).await? {
        worked = true;
    }
    Ok(worked)
}

async fn drive_conduits<S>(
    store: &S,
    now: u64,
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<bool>
where
    S: EntityStore,
{
    let jobs = store.claim_conduits(DETAIL_BATCH, now, LEASE_SECS).await?;
    if jobs.is_empty() {
        return Ok(false);
    }
    let count = jobs.len() as u64;
    crate::metrics::begin_batch(STAGE_CONDUIT, count);
    let started = std::time::Instant::now();
    let mut done = Vec::new();
    let mut again = Vec::new();
    let mut failed = Vec::new();
    for job in &jobs {
        match store.build_conduit(job.token_in, job.token_out).await {
            Ok(true) => {
                again.push((job.token_in, job.token_out));
                done.push((job.token_in, job.token_out));
            }
            Ok(false) => done.push((job.token_in, job.token_out)),
            Err(err) => {
                let error = crate::rpc::redact_secret(&err.to_string());
                if give_up_after > 0 && job.attempt >= give_up_after {
                    tracing::error!(
                        stage = STAGE_CONDUIT,
                        token_in = %job.token_in,
                        token_out = %job.token_out,
                        attempt = job.attempt,
                        %error,
                        "state gave up"
                    );
                    notes.push(format!(
                        "gave up conduit {} {}: {error}",
                        job.token_in, job.token_out
                    ));
                    done.push((job.token_in, job.token_out));
                } else {
                    tracing::warn!(
                        stage = STAGE_CONDUIT,
                        token_in = %job.token_in,
                        token_out = %job.token_out,
                        attempt = job.attempt,
                        %error,
                        "state job failed"
                    );
                    failed.push(crate::conduit::ConduitFail {
                        token_in: job.token_in,
                        token_out: job.token_out,
                        not_before: unix_now().saturating_add(RETRY_SECS),
                        error,
                    });
                }
            }
        }
    }
    store.finish_conduits(&done, &again, &failed).await?;
    let ms = started.elapsed().as_millis() as u64;
    crate::metrics::record_batch(STAGE_CONDUIT, count, ms);
    tracing::info!(stage = STAGE_CONDUIT, jobs = count, ms, "state batch");
    Ok(true)
}

async fn run_stage<S: EntityStore>(
    store: S,
    rpc: Rpc,
    stage: u8,
    jobs: Vec<WorkJob>,
    block: u64,
    now: u64,
    give_up_after: u32,
) -> eyre::Result<Vec<String>> {
    let count = jobs.len() as u64;
    crate::metrics::begin_batch(stage, count);
    let started = std::time::Instant::now();
    let mut notes = Vec::new();
    let result = match stage {
        STAGE_IMMUTABLE | STAGE_MUTABLE => {
            drive_details(&store, &rpc, block, stage, &jobs, give_up_after, &mut notes).await
        }
        STAGE_TICKS => drive_ticks(&store, &rpc, &jobs, now, give_up_after, &mut notes).await,
        STAGE_CURVE => drive_curves(&store, &jobs, give_up_after, &mut notes).await,
        _ => Ok(()),
    };
    let ms = started.elapsed().as_millis() as u64;
    crate::metrics::record_batch(stage, count, ms);
    tracing::info!(stage, jobs = count, ms, "state batch");
    result?;
    Ok(notes)
}

async fn drive_details<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    block: u64,
    stage: u8,
    jobs: &[WorkJob],
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<()> {
    let mut rows = Vec::new();
    let mut missing = Vec::new();
    for job in jobs {
        match store.read_venue(job.kind, job.id).await? {
            Some(row) => rows.push(row),
            None => missing.push((job.stage, job.kind, job.id)),
        }
    }
    if !missing.is_empty() {
        store.finish_work(&missing, &[]).await?;
    }
    if rows.is_empty() {
        return Ok(());
    }
    let mut done = Vec::new();
    let mut failed = Vec::new();
    let mut follow = Vec::new();
    let rpc = rpc.clone();
    let queries = rows.clone();
    let loaded = match tokio::task::spawn_blocking(move || {
        crate::detail::load_details_at(&rpc, &queries, Some(block))
    })
    .await
    {
        Ok(Ok(loaded)) => loaded,
        Ok(Err(err)) => {
            let error = crate::rpc::redact_secret(&format!("{err:#}"));
            for row in &rows {
                let fallback = blank_job(stage, row.kind, row.id);
                let job = job_for(jobs, row.kind, row.id).unwrap_or(&fallback);
                keep_or_drop(
                    job,
                    error.clone(),
                    give_up_after,
                    &mut done,
                    &mut failed,
                    notes,
                );
            }
            store.finish_work(&done, &failed).await?;
            return Ok(());
        }
        Err(err) => return Err(eyre::eyre!("detail task: {err}")),
    };
    if loaded.len() != rows.len() {
        return Err(eyre::eyre!("detail lens row count"));
    }
    for (row, detail) in rows.iter().zip(loaded) {
        if detail_flags(&detail) & FLAG_FAILED != 0 {
            let fallback = blank_job(stage, row.kind, row.id);
            let job = job_for(jobs, row.kind, row.id).unwrap_or(&fallback);
            keep_or_drop(
                job,
                "pool read failed".into(),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
            continue;
        }
        let (immutable, mut mutable) = match split_detail(&detail) {
            Ok(parts) => parts,
            Err(err) => {
                let fallback = blank_job(stage, row.kind, row.id);
                let job = job_for(jobs, row.kind, row.id).unwrap_or(&fallback);
                keep_or_drop(job, err, give_up_after, &mut done, &mut failed, notes);
                continue;
            }
        };
        mutable.block = block;
        if stage == STAGE_IMMUTABLE {
            store
                .put_immutable(&[ImmutableRow {
                    kind: row.kind,
                    id: row.id,
                    body: immutable,
                }])
                .await?;
            mutable.class = CLASS_NONE;
        } else {
            let state = store.read_state(row.kind, row.id).await?;
            mutable.class = state.dirty.map(|dirty| dirty.class).unwrap_or(CLASS_HEAD);
        }
        let stored = store.put_mutable(&mutable).await?;
        let mut detail = detail;
        let mut book_ready = mutable.tick_count == 0 || !concentrated_kind(row.kind);
        if stored.kept_ticks {
            let fresh = store.read_state(row.kind, row.id).await?;
            if let PoolDetail::Concentrated(pool) = &mut detail {
                pool.ticks = fresh.ticks;
            }
            book_ready = true;
        }
        let plan = follow_for_detail(&FollowInput {
            kind: row.kind,
            id: row.id,
            detail: &detail,
            class: mutable.class,
            bitmap_changed: stored.bitmap_changed && !stored.kept_ticks,
            truncated: mutable.truncated,
            tick_count: mutable.tick_count,
            generation: stored.generation,
            watched: stage == STAGE_MUTABLE,
            book_ready,
        });
        if plan.derive {
            match curve_for_snapshot(&detail, stored.generation) {
                Ok(curve) => {
                    if store.put_curve(&curve).await.is_err() {
                        let mut job = curve_job(row.kind, row.id, stored.generation);
                        job.priority = plan.priority;
                        follow.push(job);
                    }
                }
                Err(_) => {
                    let mut job = curve_job(row.kind, row.id, stored.generation);
                    job.priority = plan.priority;
                    follow.push(job);
                }
            }
        }
        if let Some(job) = plan.job {
            follow.push(job);
        }
        done.push((stage, row.kind, row.id));
    }
    store.enqueue_work(&follow, unix_now()).await?;
    store.finish_work(&done, &failed).await?;
    Ok(())
}

async fn drive_ticks<S: EntityStore>(
    store: &S,
    rpc: &Rpc,
    jobs: &[WorkJob],
    now: u64,
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<()> {
    let mut requests = Vec::new();
    let mut metas = Vec::new();
    let mut done = Vec::new();
    let mut failed = Vec::new();
    let mut follow = Vec::new();
    for job in jobs {
        let state = store.read_state(job.kind, job.id).await?;
        let Some(immutable) = state.immutable else {
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        let Some(mutable) = state.mutable else {
            follow.push(blank_job(STAGE_MUTABLE, job.kind, job.id));
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        if job.generation != 0 && job.generation != mutable.generation {
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        let detail = match join_detail(&immutable, &mutable.body, &[], job.kind, job.id) {
            Ok(detail) => detail,
            Err(err) => {
                keep_or_drop(job, err, give_up_after, &mut done, &mut failed, notes);
                continue;
            }
        };
        let PoolDetail::Concentrated(pool) = detail else {
            if let Some(next) = after_book(store, job).await? {
                follow.push(next);
            }
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        let ticks = pool.initialized_ticks();
        let page = u32::try_from(job.tick_cursor.max(0)).unwrap_or(0);
        let start = page.saturating_mul(TICK_PAGE) as usize;
        if start >= ticks.len() {
            if let Some(next) = after_book(store, job).await? {
                follow.push(next);
            }
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        let end = (start + TICK_PAGE as usize).min(ticks.len());
        requests.push(tick_request(&pool, ticks[start..end].to_vec()));
        metas.push((
            job.clone(),
            mutable.generation,
            mutable.block,
            page,
            end < ticks.len(),
        ));
    }
    if requests.is_empty() {
        store.enqueue_work(&follow, now).await?;
        store.finish_work(&done, &failed).await?;
        return Ok(());
    }
    let mut groups: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (index, meta) in metas.iter().enumerate() {
        groups.entry(meta.2).or_default().push(index);
    }
    let mut words_by_index = vec![Vec::new(); metas.len()];
    let mut failed_by_index = vec![false; metas.len()];
    let mut errors_by_index = vec![None; metas.len()];
    for (block, indexes) in groups {
        let batch: Vec<_> = indexes
            .iter()
            .map(|index| requests[*index].clone())
            .collect();
        let rpc = rpc.clone();
        let loaded =
            match tokio::task::spawn_blocking(move || tick::load_ticks(&rpc, &batch, Some(block)))
                .await
            {
                Ok(Ok(loaded)) => loaded,
                Ok(Err(err)) => {
                    let error = crate::rpc::redact_secret(&format!("{err:#}"));
                    for index in &indexes {
                        errors_by_index[*index] = Some(error.clone());
                    }
                    continue;
                }
                Err(err) => return Err(eyre::eyre!("tick task: {err}")),
            };
        if loaded.ticks.len() != indexes.len() || loaded.failed.len() != indexes.len() {
            return Err(eyre::eyre!("tick lens row count"));
        }
        for (slot, index) in indexes.into_iter().enumerate() {
            words_by_index[index] = loaded.ticks[slot].clone();
            failed_by_index[index] = loaded.failed[slot];
        }
    }
    for (index, (job, generation, _block, page, more)) in metas.into_iter().enumerate() {
        let words = std::mem::take(&mut words_by_index[index]);
        if let Some(error) = errors_by_index[index].clone() {
            keep_or_drop(&job, error, give_up_after, &mut done, &mut failed, notes);
            continue;
        }
        let failed_read = failed_by_index[index];
        if failed_read {
            keep_or_drop(
                &job,
                "tick read failed".into(),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
            continue;
        }
        let ticks = words
            .into_iter()
            .map(|TickWordRead { tick, word }| {
                let (liquidity_gross, liquidity_net) = unpack_stored(word);
                TickWord {
                    tick,
                    liquidity_gross,
                    liquidity_net,
                }
            })
            .collect();
        store
            .put_tick_pages(&[TickPage {
                kind: job.kind,
                id: job.id,
                page,
                generation,
                ticks,
            }])
            .await?;
        if more {
            let mut again = job.clone();
            again.lease_until = 0;
            again.not_before = 0;
            again.tick_cursor = page.saturating_add(1) as i32;
            again.generation = generation;
            follow.push(again);
        } else if let Some(next) = after_book(store, &job).await? {
            follow.push(next);
        }
        done.push((job.stage, job.kind, job.id));
    }
    // The next tick page reuses this job's key. Finish first, or that insert is deleted.
    store.finish_work(&done, &failed).await?;
    store.enqueue_work(&follow, now).await?;
    Ok(())
}

async fn drive_curves<S: EntityStore>(
    store: &S,
    jobs: &[WorkJob],
    give_up_after: u32,
    notes: &mut Vec<String>,
) -> eyre::Result<()> {
    let mut done = Vec::new();
    let mut failed = Vec::new();
    for job in jobs {
        let state = store.read_state(job.kind, job.id).await?;
        let (Some(immutable), Some(mutable)) = (state.immutable, state.mutable) else {
            done.push((job.stage, job.kind, job.id));
            continue;
        };
        if job.generation != 0 && job.generation != mutable.generation {
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        let detail = match join_detail(&immutable, &mutable.body, &state.ticks, job.kind, job.id) {
            Ok(detail) => detail,
            Err(err) => {
                keep_or_drop(job, err, give_up_after, &mut done, &mut failed, notes);
                continue;
            }
        };
        let cap = crate::fit::trade_capacity(&detail);
        if !cap.any_positive {
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        if curve_is_current(state.curve.as_ref(), mutable.generation) {
            done.push((job.stage, job.kind, job.id));
            continue;
        }
        let curve = match curve_for_snapshot(&detail, mutable.generation) {
            Ok(curve) => curve,
            Err(err) => {
                keep_or_drop(job, err, give_up_after, &mut done, &mut failed, notes);
                continue;
            }
        };
        if let Err(err) = store.put_curve(&curve).await {
            keep_or_drop(
                job,
                err.to_string(),
                give_up_after,
                &mut done,
                &mut failed,
                notes,
            );
            continue;
        }
        done.push((job.stage, job.kind, job.id));
    }
    store.finish_work(&done, &failed).await?;
    Ok(())
}

async fn watch_once<S: EntityStore, P: HistoryPeer>(
    store: &S,
    peer: &P,
) -> eyre::Result<Option<String>> {
    let head = store.header_cursor().await?;
    if head == 0 {
        return Ok(None);
    }
    if store.watch_through().await?.is_none() {
        store.apply_watch(head, &[]).await?;
        return Ok(Some(format!("watch armed at {head}")));
    }
    let from = store.watch_through().await?.unwrap_or(0);
    if head <= from {
        return Ok(None);
    }
    let count = (head - from).min(8);
    let blocks = peer.chunk(from, count).await?;
    if blocks.is_empty() {
        return Ok(None);
    }
    if !contiguous(&blocks, from) {
        return Err(eyre::eyre!("watch chunk did not start at {from}"));
    }
    let mut hits = Vec::new();
    for block in &blocks {
        let logs = corpus::logs_in_block(block)?;
        for log in logs {
            if let Some(hit) = watch_hit(store, &log).await? {
                hits.push(hit);
            }
        }
    }
    let through = from + blocks.len() as u64;
    store.apply_watch(through, &hits).await?;
    Ok(Some(format!("watch {from}..{through} hits {}", hits.len())))
}

async fn watch_hit<S: EntityStore>(
    store: &S,
    log: &NormalizedLog,
) -> eyre::Result<Option<WatchHit>> {
    let Some(&topic) = log.topics.first() else {
        return Ok(None);
    };
    let known = crate::decode::known();
    if log.address == known.pool_manager || log.address == known.bal_v2 {
        let (kind, head, other, _other_class) = if log.address == known.pool_manager {
            (UNI_V4, TOPIC_V4_SWAP, TOPIC_V4_LIQUIDITY, CLASS_TICKS)
        } else {
            (BALANCER_V2, TOPIC_BAL_SWAP, TOPIC_BAL_BALANCE, CLASS_HEAD)
        };
        if topic != head && topic != other {
            return Ok(None);
        }
        let Some(id) = topic_id(log) else {
            return Ok(None);
        };
        let present = store.read_venue(kind, id).await?.is_some();
        return Ok(classify_log(
            log,
            |asked_kind, asked_id| asked_kind == kind && asked_id == id && present,
            |_| None,
        ));
    }
    if log.address == known.bal_v3 && topic == TOPIC_BAL3_SWAP {
        let Some(id) = topic_id(log) else {
            return Ok(None);
        };
        let present = store.read_venue(BALANCER_V3, id).await?.is_some();
        return Ok(classify_log(
            log,
            |asked_kind, asked_id| asked_kind == BALANCER_V3 && asked_id == id && present,
            |_| None,
        ));
    }
    if log.address == known.bal_v3 {
        return Ok(None);
    }
    let Some((kind, id)) = find_address(store, log.address).await? else {
        return Ok(None);
    };
    Ok(classify_log(log, |_, _| false, |_| Some((kind, id))))
}

async fn find_address<S: EntityStore>(
    store: &S,
    address: Address,
) -> eyre::Result<Option<(u8, B256)>> {
    let id = VenueRow::address_id(address);
    for kind in 1..=METRIC {
        if !(concentrated_kind(kind)
            || matches!(kind, 1 | 2 | 3 | 4 | AERO | CURVE | METRIC | BALANCER_V3))
        {
            continue;
        }
        if store.read_venue(kind, id).await?.is_some() {
            return Ok(Some((kind, id)));
        }
    }
    Ok(None)
}

fn contiguous(blocks: &[BlockRecord], from: u64) -> bool {
    blocks
        .iter()
        .enumerate()
        .all(|(index, block)| block.number == from + index as u64)
}

#[cfg(test)]
mod tests {
    use super::tagged_block;

    #[test]
    fn the_read_block_follows_the_header_cursor() {
        assert_eq!(tagged_block(None, 0), None);
        assert_eq!(tagged_block(None, 1), Some(0));
        assert_eq!(tagged_block(None, 50), Some(49));
        assert_eq!(tagged_block(Some(7), 50), Some(7));
        assert_eq!(tagged_block(Some(7), 0), Some(7));
    }
}
