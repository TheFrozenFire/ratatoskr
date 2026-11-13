//! Venue identities for the mainnet pools people trade.
//!
//! A list view (`allPairs`, `allPools`, `pool_list`, `idxToPool`) is read by
//! `eth_call`ing the creation code in `contracts/lens/IndexLens.sol` with no
//! `to`. The constructor reverts the rows. Any client that runs a creation
//! `eth_call` can serve it. Factories with no list are found in the local
//! header blooms. Hits stay in the venue catalog. Receipts come from era1
//! before the merge and from the peer daemon after it, and those payloads
//! stay in redb.

mod catalog;
mod conduit;
mod decode;
mod detail;
mod entity;
mod fit;
mod fit_formula;
mod fit_math;
mod fit_select;
mod history;
mod import;
mod ingest;
mod lens;
mod lists;
mod logs;
mod metrics;
mod rpc;
mod seeds;
mod state;
mod tick;
mod wire;

pub use wire::{
    decode_state_call, decode_state_reply, encode_state_call, encode_state_reply, StateCall,
    StateReply,
};

pub use catalog::{
    append_dropped_ids, append_fill_ids, decode_body, decode_row, encode_body, encode_row,
    fill_identities, kind_name, read_fill_ids, sample_identities, write_fill_ids, Catalog,
    FillReport, Put, ReceiptBody, SampleReport, TokenRecord, VenueRow, BALANCER_V2, BALANCER_V3,
    PANCAKE_V3, SUSHI_V3, UNI_V3, UNI_V4, V3_FORK,
};
pub use conduit::{ConduitFail, ConduitJob};
pub use detail::{load_details, DetailQuery, PoolDetail, Quote};
pub use entity::EntityStore;
pub use fit::{drop_partial_quotes, export_curve_fit, import_curve_fit, refit_curves, FitReport};
pub use fit_select::{logistic_outs, poke_choose, poke_formula, select_fit};
pub use history::BloomScan;
pub use logs::Walk;
pub use metrics::{prometheus_text, publish_summary, status_json};
pub use rpc::Rpc;
pub use seeds::root_address;
pub use state::{
    estimate, CurveRecord, ImmutablePut, ImmutableRow, MutableStored, MutableWrite, PoolState,
    QuoteRecord, StateSummary, TickPage, WatchHit, WorkFail, WorkJob,
};

use ipc::HistoryPeer;
use std::sync::OnceLock;
use tokio::sync::watch;

struct WorkPause {
    tx: watch::Sender<bool>,
    _rx: watch::Receiver<bool>,
}

static WORK_PAUSE: OnceLock<WorkPause> = OnceLock::new();

fn work_pause() -> &'static WorkPause {
    WORK_PAUSE.get_or_init(|| {
        let (tx, rx) = watch::channel(false);
        WorkPause { tx, _rx: rx }
    })
}

/// Stop claiming work on every stage. Peer sessions stay connected.
pub fn pause_work() {
    let _ = work_pause().tx.send(true);
    tracing::info!("work paused");
}

/// Let every stage claim again. Peer sessions were not dropped.
pub fn resume_work() {
    let _ = work_pause().tx.send(false);
    tracing::info!("work resumed");
}

pub fn work_paused() -> bool {
    *work_pause().tx.borrow()
}

/// Wait while work is paused. Returns true when the caller should shut down.
pub async fn hold_for_pause(stop: &watch::Receiver<bool>) -> bool {
    let mut paused = work_pause().tx.subscribe();
    let mut stop = stop.clone();
    loop {
        if *stop.borrow() {
            return true;
        }
        if !*paused.borrow() {
            return false;
        }
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return true;
                }
            }
            changed = paused.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
        }
    }
}

/// Which part of a venue run to perform. A standalone binary splits these so
/// one phase does not hold a file the next phase must leave free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunPhase {
    Lists,
    Corpus,
    ListsAndCorpus,
    History,
    /// Pool details, tick pages, curves, and the watch. Not part of [`Self::All`].
    State,
    All,
}

#[derive(Clone)]
pub struct RunOpts {
    pub rpcs: Vec<String>,
    pub only: Option<String>,
    pub no_corpus: bool,
    /// The caller opened the corpus file this import should scan.
    pub corpus_available: bool,
    pub phase: RunPhase,
    /// `eth_call` block. When set, state does not wait on the header cursor.
    pub pin_block: Option<u64>,
    /// Leave the state loop once nothing is left to claim. A job that fails
    /// four times is dropped and recorded.
    pub until_idle: bool,
}

#[derive(Debug, Clone, Default)]
pub struct RunReport {
    pub list_errors: Vec<String>,
    pub notes: Vec<String>,
}

pub fn job_wanted(only: &Option<String>, name: &str) -> bool {
    match only {
        None => true,
        Some(part) => name.to_lowercase().contains(&part.to_lowercase()),
    }
}

pub fn history_selected(only: &Option<String>) -> bool {
    seeds::document()
        .roots
        .iter()
        .filter(|root| root.logs())
        .any(|root| job_wanted(only, &format!("logs:{}", root.name)))
}

pub async fn execute<S, P>(
    store: &S,
    peer: Option<&P>,
    opts: &RunOpts,
    stop: tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<RunReport>
where
    S: EntityStore + Clone + 'static,
    P: HistoryPeer + Clone + 'static,
{
    if opts.phase == RunPhase::State {
        return state::run(store, peer, opts, stop).await;
    }
    let mut report = RunReport::default();
    let lists = matches!(
        opts.phase,
        RunPhase::Lists | RunPhase::ListsAndCorpus | RunPhase::All
    );
    let import = matches!(
        opts.phase,
        RunPhase::Corpus | RunPhase::ListsAndCorpus | RunPhase::All
    );
    let history = matches!(opts.phase, RunPhase::History | RunPhase::All);
    let calls = if lists || history {
        match open_calls(opts).await {
            Ok(calls) => Some(calls),
            Err(err) => {
                let message = format!("seeds: {err:#}");
                tracing::error!(%message);
                report.list_errors.push(message);
                None
            }
        }
    } else {
        None
    };
    let planned = if let Some(calls) = &calls {
        match ingest::plan(store, calls).await {
            Ok(plan) => Some(plan),
            Err(err) => {
                let message = format!("seeds: {err:#}");
                tracing::error!(%message);
                report.list_errors.push(message);
                None
            }
        }
    } else {
        None
    };
    if lists {
        if let (Some(calls), Some(plan)) = (&calls, &planned) {
            for job in &plan.lists {
                if hold_for_pause(&stop).await {
                    return Ok(report);
                }
                if !job_wanted(&opts.only, &job.name) {
                    continue;
                }
                match lists::walk(calls, store, job, &stop).await {
                    Ok(wrote) => tracing::info!(job = %job.name, wrote, "list finished"),
                    Err(err) => {
                        let message = format!("{}: {err}", job.name);
                        tracing::error!(%message, "list failed");
                        report.list_errors.push(message);
                    }
                }
            }
        }
    }
    if import && !opts.no_corpus && opts.corpus_available && job_wanted(&opts.only, "corpus") {
        if hold_for_pause(&stop).await {
            return Ok(report);
        }
        match import::import_corpus(store).await {
            Ok(wrote) => {
                let note = format!("corpus wrote {wrote}");
                tracing::info!(%note);
                report.notes.push(note);
            }
            Err(err) => {
                let message = format!("corpus: {err}");
                tracing::error!(%message);
                report.list_errors.push(message);
            }
        }
    }
    if history {
        if hold_for_pause(&stop).await {
            return Ok(report);
        }
        let logs_wanted = planned
            .as_ref()
            .map(|plan| {
                plan.logs
                    .iter()
                    .filter(|job| job_wanted(&opts.only, &job.name))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if !logs_wanted.is_empty() {
            match history::ingest_all(store, peer, &logs_wanted, &stop).await {
                Ok(notes) => report.notes.extend(notes),
                Err(err) => {
                    let note = format!("history: {err:#}");
                    tracing::error!(%note);
                    report.notes.push(note);
                }
            }
        }
    }
    Ok(report)
}

/// Bloom scan. The caller opens `headers.redb` for this and drops it before
/// [`fetch_receipts`]. The corpus stays closed so an ingest lock does not
/// block the scan.
pub async fn scan_blooms<S: EntityStore>(
    store: &S,
    only: &Option<String>,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<history::BloomScan> {
    let jobs = ingest::cached_logs(store)
        .await
        .into_iter()
        .filter(|job| job_wanted(only, &job.name))
        .collect::<Vec<_>>();
    history::scan_blooms(store, &jobs, stop).await
}

async fn open_calls(opts: &RunOpts) -> eyre::Result<rpc::Rpc> {
    let call_urls = if opts.rpcs.is_empty() {
        rpc::default_call_rpcs()
    } else {
        opts.rpcs.clone()
    };
    tokio::task::spawn_blocking(move || rpc::Rpc::new(call_urls))
        .await
        .map_err(|err| eyre::eyre!("rpc client task: {err}"))?
}

/// Receipt fetch for a finished bloom scan. Needs the corpus and the catalog,
/// not the header file.
pub async fn fetch_receipts<S, P>(
    store: &S,
    peer: Option<&P>,
    scan: history::BloomScan,
    stop: &tokio::sync::watch::Receiver<bool>,
) -> eyre::Result<Vec<String>>
where
    S: EntityStore,
    P: HistoryPeer + Clone + 'static,
{
    history::fetch_receipts(store, peer, scan, stop).await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn one_at_a_time() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    struct ResumeOnDrop;
    impl Drop for ResumeOnDrop {
        fn drop(&mut self) {
            resume_work();
        }
    }

    #[tokio::test]
    async fn pause_work_parks_until_resume() {
        let _one = one_at_a_time().await;
        let _guard = ResumeOnDrop;
        let (_stop_tx, stop) = watch::channel(false);
        pause_work();
        assert!(work_paused());
        let wait = tokio::spawn(async move { hold_for_pause(&stop).await });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        assert!(!wait.is_finished());
        resume_work();
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .expect("resume wakes the stage")
            .expect("task");
        assert!(!stopped);
        assert!(!work_paused());
    }

    #[tokio::test]
    async fn pause_work_still_hears_shutdown() {
        let _one = one_at_a_time().await;
        let _guard = ResumeOnDrop;
        let (stop_tx, stop) = watch::channel(false);
        pause_work();
        let wait = tokio::spawn(async move { hold_for_pause(&stop).await });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        stop_tx.send(true).expect("shutdown");
        let stopped = tokio::time::timeout(std::time::Duration::from_secs(1), wait)
            .await
            .expect("shutdown wakes the stage")
            .expect("task");
        assert!(stopped);
    }
}
